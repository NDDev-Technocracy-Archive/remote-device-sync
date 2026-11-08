use std::sync::Arc;

use winit::{dpi::PhysicalSize, window::Window};

use crate::{DesktopError, RawFrame};

use super::input::Viewport;
use super::observation::{DrawObservation, DrawUpdate, Phase, RendererInfo};
use super::workspace::chrome::UiFrame;

const SHADER: &str = r#"
struct Vertex { @builtin(position) position: vec4f, @location(0) uv: vec2f }
@vertex fn vertex(@builtin(vertex_index) index: u32) -> Vertex {
    let p = array<vec2f, 3>(vec2f(-1., -1.), vec2f(3., -1.), vec2f(-1., 3.));
    var v: Vertex;
    v.position = vec4f(p[index], 0., 1.);
    v.uv = vec2f((p[index].x + 1.) * .5, (1. - p[index].y) * .5);
    return v;
}
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var filtering: sampler;
@fragment fn fragment(v: Vertex) -> @location(0) vec4f {
    return vec4f(textureSample(picture, filtering, v.uv).rgb, 1.);
}
"#;

fn error(e: impl std::fmt::Display) -> DesktopError {
    DesktopError::Capture(format!("native renderer: {e}"))
}

#[derive(Clone, Copy)]
pub(super) enum DrawOutcome {
    Presented,
    Empty,
    Occluded,
    TimedOut,
    Reconfigured,
}
impl DrawOutcome {
    pub(super) fn stage(self) -> &'static str {
        match self {
            Self::Presented => "presented",
            Self::Empty => "waiting for picture",
            Self::Occluded => "surface occluded",
            Self::TimedOut => "surface timeout",
            Self::Reconfigured => "surface reconfigured",
        }
    }
}

pub(super) struct Gpu {
    info: RendererInfo,
    instance: wgpu::Instance,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    picture: Option<(wgpu::Texture, wgpu::BindGroup, u32, u32)>,
    uploads: u64,
    chrome: Option<egui_wgpu::Renderer>,
    reconfigure: bool,
}

impl Gpu {
    pub(super) fn new(
        window: Arc<Window>,
        display: winit::event_loop::OwnedDisplayHandle,
    ) -> Result<Self, DesktopError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        ));
        let surface = instance.create_surface(window.clone()).map_err(error)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(error)?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(error)?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| error("no supported surface configuration"))?;
        // AutoNoVsync prefers Immediate and AutoVsync may use FifoRelaxed;
        // both allow tearing through the local UI as video arrives. Mailbox
        // retains the newest frame without tearing; Fifo is the portable floor.
        config.present_mode =
            coherent_present_mode(&surface.get_capabilities(&adapter).present_modes);
        config.desired_maximum_frame_latency = 1;
        if let Some(format) = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(wgpu::TextureFormat::is_srgb)
        {
            config.format = format;
        }
        surface.configure(&device, &config);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RDS BGRA presentation"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("RDS native presentation"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("RDS screen scaling"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        tracing::info!(backend = ?adapter.get_info().backend, present_mode = ?config.present_mode,
            "native desktop renderer ready");
        let info = RendererInfo {
            backend: format!("{:?}", adapter.get_info().backend),
            device_type: format!("{:?}", adapter.get_info().device_type),
            present_mode: format!("{:?}", config.present_mode),
            surface_format: format!("{:?}", config.format),
            desired_maximum_frame_latency: config.desired_maximum_frame_latency,
        };
        Ok(Self {
            info,
            instance,
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            sampler,
            picture: None,
            uploads: 0,
            chrome: None,
            reconfigure: false,
        })
    }

    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.reconfigure = false;
    }

    fn upload(&mut self, frame: &RawFrame) -> Result<(), DesktopError> {
        let row = frame
            .width
            .checked_mul(4)
            .ok_or_else(|| error("invalid frame width"))?;
        let required = (frame.stride as usize)
            .checked_mul(frame.height as usize)
            .ok_or_else(|| error("invalid frame stride"))?;
        if crate::frame_bytes(frame.width as usize, frame.height as usize).is_none()
            || frame.stride < row
            || frame.data.len() < required
        {
            return Err(error("invalid BGRA frame"));
        }
        if self
            .picture
            .as_ref()
            .is_none_or(|(_, _, w, h)| (*w, *h) != (frame.width, frame.height))
        {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("RDS latest screen"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if self.config.format.is_srgb() {
                    wgpu::TextureFormat::Bgra8UnormSrgb
                } else {
                    wgpu::TextureFormat::Bgra8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("RDS screen"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.picture = Some((texture, group, frame.width, frame.height));
        }
        if let Some((texture, _, _, _)) = &self.picture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame.data[..required],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.stride),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.uploads += 1;
        Ok(())
    }

    pub(super) fn uploads(&self) -> u64 {
        self.uploads
    }
    pub(super) fn info(&self) -> &RendererInfo {
        &self.info
    }

    pub(super) fn clear_picture(&mut self) {
        self.picture = None;
    }

    pub(super) fn has_picture(&self) -> bool {
        self.picture.is_some()
    }

    pub(super) fn draw(
        &mut self,
        frame: Option<&RawFrame>,
        mut ui: Option<&mut UiFrame>,
        mut observe: impl FnMut(DrawUpdate),
    ) -> Result<DrawOutcome, DesktopError> {
        let mut observation = DrawObservation::new(&mut observe);
        observation.enter(Phase::Configuration);
        // A suboptimal surface remains usable for this frame, but its backing
        // must be refreshed before acquiring another image (e.g. a DPI move).
        if self.reconfigure {
            self.resize(self.window.inner_size());
        }
        observation.enter(Phase::Acquisition);
        let surface = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                self.reconfigure = true;
                frame
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                observation.enter(Phase::Configuration);
                self.surface = self
                    .instance
                    .create_surface(self.window.clone())
                    .map_err(error)?;
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(DrawOutcome::Reconfigured);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                observation.enter(Phase::Configuration);
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(DrawOutcome::Reconfigured);
            }
            wgpu::CurrentSurfaceTexture::Timeout => return Ok(DrawOutcome::TimedOut),
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(DrawOutcome::Occluded),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(error("surface validation failed"));
            }
        };
        // Queue writes retain staging allocations until submit. Never upload
        // while an occluded/unavailable surface cannot submit: one retained
        // CPU frame must not become an unbounded queue of GPU upload buffers.
        if let Some(frame) = frame {
            observation.enter(Phase::Upload);
            self.upload(frame)?;
        }
        observation.enter(Phase::Commands);
        let view = surface.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let screen = ui.as_ref().map(|ui| egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: ui.pixels_per_point,
        });
        let mut commands = vec![];
        if let (Some(ui), Some(screen)) = (ui.as_mut(), screen.as_ref()) {
            let renderer = self.chrome.get_or_insert_with(|| {
                egui_wgpu::Renderer::new(
                    &self.device,
                    self.config.format,
                    egui_wgpu::RendererOptions::default(),
                )
            });
            for (id, deltas) in &ui.textures.set {
                for delta in deltas {
                    renderer.update_texture(&self.device, &self.queue, *id, delta);
                }
            }
            ui.textures.set.clear();
            commands = renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &ui.primitives,
                screen,
            );
        }
        let mut picture_presented = false;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("RDS screen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some((_, group, width, height)) = &self.picture {
                let area = ui.as_ref().map(|ui| {
                    [
                        f64::from(ui.content.left()),
                        f64::from(ui.content.top()),
                        f64::from(ui.content.width()),
                        f64::from(ui.content.height()),
                    ]
                });
                let viewport =
                    Viewport::content(self.config.width, self.config.height, *width, *height, area);
                if viewport.width > 0. && viewport.height > 0. {
                    pass.set_viewport(
                        viewport.x as f32,
                        viewport.y as f32,
                        viewport.width as f32,
                        viewport.height as f32,
                        0.,
                        1.,
                    );
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, group, &[]);
                    pass.draw(0..3, 0..1);
                    picture_presented = true;
                }
            }
        }
        if let (Some(renderer), Some(ui), Some(screen)) =
            (&self.chrome, ui.as_ref(), screen.as_ref())
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("RDS workspace"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&mut pass.forget_lifetime(), &ui.primitives, screen);
        }
        commands.push(encoder.finish());
        observation.enter(Phase::Submission);
        self.queue.submit(commands);
        observation.enter(Phase::Cleanup);
        if let (Some(renderer), Some(ui)) = (&mut self.chrome, ui.as_mut()) {
            for id in ui.textures.free.drain() {
                renderer.free_texture(&id);
            }
        }
        self.window.pre_present_notify();
        observation.enter(Phase::Presentation);
        self.queue.present(surface);
        observation.enter(Phase::Cleanup);
        if self.reconfigure {
            self.window.request_redraw();
        }
        Ok(if picture_presented {
            DrawOutcome::Presented
        } else {
            DrawOutcome::Empty
        })
    }
}

fn coherent_present_mode(supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    if supported.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_never_chooses_a_tearing_mode() {
        use wgpu::PresentMode::{AutoNoVsync, AutoVsync, Fifo, FifoRelaxed, Immediate, Mailbox};
        assert_eq!(coherent_present_mode(&[Immediate, Fifo]), Fifo);
        assert_eq!(coherent_present_mode(&[FifoRelaxed, Fifo]), Fifo);
        assert_eq!(coherent_present_mode(&[Fifo, Mailbox, Immediate]), Mailbox);
        assert_eq!(coherent_present_mode(&[AutoNoVsync, AutoVsync, Fifo]), Fifo);
    }
}
