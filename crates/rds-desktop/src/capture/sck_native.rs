//! ScreenCaptureKit objects stay on their native owner worker. Only immutable
//! inventory and owned BGRA images cross the service boundary.
#![allow(unsafe_code)]

use super::{
    geometry::Geometry,
    latest::{Image, Slot},
};
use crate::{DesktopError, RawFrame};
use block2::RcBlock;
use bytes::Bytes;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive, CGDisplayMode, CGDisplayRotation,
    CGMainDisplayID, CGPreflightScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::*;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) fn error() -> DesktopError {
    DesktopError::Capture("ScreenCaptureKit screen unavailable or local permission missing".into())
}

pub(crate) fn geometry(id: u32) -> Result<Geometry, DesktopError> {
    if !CGDisplayIsActive(id) {
        return Err(error());
    }
    let bounds = CGDisplayBounds(id);
    let mode = CGDisplayCopyDisplayMode(id).ok_or_else(error)?;
    let mut width = u32::try_from(CGDisplayMode::pixel_width(Some(&mode))).map_err(|_| error())?;
    let mut height =
        u32::try_from(CGDisplayMode::pixel_height(Some(&mode))).map_err(|_| error())?;
    let rotation = CGDisplayRotation(id);
    if rotation == 90.0 || rotation == 270.0 {
        std::mem::swap(&mut width, &mut height);
    }
    let g = Geometry {
        id,
        width,
        height,
        x: bounds.origin.x,
        y: bounds.origin.y,
        logical_width: bounds.size.width,
        logical_height: bounds.size.height,
        primary: id == CGMainDisplayID(),
    };
    crate::frame_bytes(width as usize, height as usize).ok_or_else(error)?;
    g.point(0.0, 0.0).ok_or_else(error)?;
    Ok(g)
}

struct Snapshot(Retained<SCShareableContent>);
// SAFETY: SCShareableContent is an immutable retained inventory snapshot.
// It and its immutable SCDisplay entries expose no mutation; Objective-C
// retain/release is atomic. Only this snapshot crosses the completion queue;
// mutable SCStream/SCStreamConfiguration objects never leave their owner.
unsafe impl Send for Snapshot {}

fn inventory() -> Result<Snapshot, DesktopError> {
    if !CGPreflightScreenCaptureAccess() {
        return Err(error());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, failure: *mut NSError| {
            // SAFETY: Apple lends either a valid inventory or null for this callback.
            // Retain before leaving the callback; failed/timed-out receivers drop it.
            let content = if failure.is_null() {
                unsafe { Retained::retain(content) }.map(Snapshot)
            } else {
                None
            };
            let _ = tx.send(content);
        },
    );
    // SAFETY: The block has the documented ABI and Apple copies it for the
    // asynchronous call. Its channel owns all state even after a timeout.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, true, &completion);
    }
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| error())?
        .ok_or_else(error)
}

struct Pixels {
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

struct PixelLock<'a>(&'a CVPixelBuffer);
impl Drop for PixelLock<'_> {
    fn drop(&mut self) {
        // SAFETY: This guard follows a successful matching read-only lock and
        // its retained sample/image buffer remains alive until after this drop.
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}

enum Update {
    Frame(RawFrame),
    Unchanged,
    Empty,
}

fn copy_image(
    sample: &CMSampleBuffer,
    g: Geometry,
    budget: &Arc<Semaphore>,
    needs_pixels: bool,
) -> Result<Update, DesktopError> {
    // SAFETY: ScreenCaptureKit's per-sample attachment array is toll-free
    // bridged to NSArray<NSDictionary<SCStreamFrameInfo, NSNumber>>. The
    // retained array outlives these borrows and keys/values are checked by type.
    let status = unsafe {
        let Some(array) = sample.sample_attachments_array(false) else {
            return Ok(Update::Unchanged);
        };
        let array = &*std::ptr::from_ref(&*array)
            .cast::<NSArray<NSDictionary<SCStreamFrameInfo, NSNumber>>>();
        if array.count() != 1 {
            return Ok(Update::Unchanged);
        }
        array
            .objectAtIndex(0)
            .objectForKey(SCStreamFrameInfoStatus)
            .map(|number| number.integerValue())
    };
    if status.is_some_and(|value| {
        [
            SCFrameStatus::Blank.0,
            SCFrameStatus::Suspended.0,
            SCFrameStatus::Stopped.0,
        ]
        .contains(&value)
    }) {
        return Ok(Update::Empty);
    }
    if status != Some(SCFrameStatus::Complete.0) || !needs_pixels {
        return Ok(Update::Unchanged);
    }
    // SAFETY: The sample is live for the callback; image_buffer retains its
    // native image. Read-only locking and exact row bounds precede pixel access.
    let buffer = unsafe { sample.image_buffer() }.ok_or_else(error)?;
    if CVPixelBufferGetWidth(&buffer) != g.width as usize
        || CVPixelBufferGetHeight(&buffer) != g.height as usize
        || CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA
        || CVPixelBufferIsPlanar(&buffer)
    {
        return Err(error());
    }
    let size = crate::frame_bytes(g.width as usize, g.height as usize).ok_or_else(error)?;
    let Ok(permit) = budget.clone().try_acquire_many_owned(size as u32) else {
        return Ok(Update::Unchanged);
    };
    // SAFETY: This is a retained CVPixelBuffer and a supported read-only flag.
    if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return Err(error());
    }
    let _lock = PixelLock(&buffer);
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let row = g.width as usize * 4;
    let needed = (g.height as usize - 1)
        .checked_mul(stride)
        .and_then(|n| n.checked_add(row))
        .ok_or_else(error)?;
    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    if base.is_null() || stride < row || needed > CVPixelBufferGetDataSize(&buffer) {
        return Err(error());
    }
    let mut bytes = vec![0; size];
    for (y, output) in bytes.chunks_exact_mut(row).enumerate() {
        // SAFETY: The locked BGRA plane owns `needed` bytes as checked above.
        // Only initialized pixel bytes are read; native row padding is excluded.
        output.copy_from_slice(unsafe { std::slice::from_raw_parts(base.add(y * stride), row) });
    }
    Ok(Update::Frame(RawFrame {
        width: g.width,
        height: g.height,
        stride: g.width * 4,
        data: Bytes::from_owner(Pixels {
            bytes,
            _permit: permit,
        }),
    }))
}

struct OutputState {
    slot: Arc<Slot>,
    geometry: Geometry,
    budget: Arc<Semaphore>,
}
define_class!(
    // SAFETY: NSObject has no subclassing requirements; all ivars are Send +
    // Sync Rust state. Callback methods touch them only through atomics/locks.
    #[unsafe(super = NSObject)]
    #[ivars = OutputState]
    struct Output;
    // SAFETY: NSObjectProtocol adds no requirements.
    unsafe impl NSObjectProtocol for Output {}
    // SAFETY: Signatures match SCStreamOutput; the sample is borrowed only
    // during this callback, and no borrowed native storage escapes it.
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn frame(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            autoreleasepool(|_| {
                let state = self.ivars();
                let needs_pixels = match state.slot.state.lock() {
                    Ok(current) if !current.failed => {
                        current.image.is_none() || state.slot.readers.load(Ordering::Acquire) > 0
                    }
                    _ => return,
                };
                if geometry(state.geometry.id).ok() != Some(state.geometry)
                    || !CGPreflightScreenCaptureAccess()
                {
                    state.slot.fail();
                    return;
                }
                match copy_image(sample, state.geometry, &state.budget, needs_pixels) {
                    Ok(Update::Frame(frame)) => {
                        if let Ok(mut current) = state.slot.state.lock()
                            && !current.failed
                        {
                            current.generation = current.generation.saturating_add(1);
                            current.image = Some(Arc::new(Image {
                                frame,
                                received: Instant::now(),
                                generation: current.generation,
                            }));
                        }
                        state.slot.changed.notify_all();
                    }
                    Ok(Update::Empty) => {
                        if let Ok(mut current) = state.slot.state.lock() {
                            current.image = None;
                        }
                        state.slot.changed.notify_all();
                    }
                    Ok(Update::Unchanged) => {}
                    Err(_) => state.slot.fail(),
                }
            });
        }
    }
    // SAFETY: The delegate callback's signature matches Apple's protocol and
    // sealing the slot prevents new pixels and wakes all existing readers.
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stopped(&self, _stream: &SCStream, _error: &NSError) {
            self.ivars().slot.fail();
        }
    }
);

fn completed(stream: &SCStream, start: bool) -> Result<(), DesktopError> {
    let (tx, rx) = mpsc::sync_channel(1);
    let completion = RcBlock::new(move |failure: *mut NSError| {
        let _ = tx.send(failure.is_null());
    });
    // SAFETY: Correct completion ABI, state is channel-owned and Apple copies
    // the block. Mutable native objects stay on this worker until completion.
    unsafe {
        if start {
            stream.startCaptureWithCompletionHandler(Some(&completion));
        } else {
            stream.stopCaptureWithCompletionHandler(Some(&completion));
        }
    }
    // A running native operation is retained until its real completion. A slow
    // OS call must not be acknowledged as drained by timing out its JoinHandle.
    let result = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(start, "ScreenCaptureKit completion still pending");
            rx.recv().map_err(|_| error())?
        }
        Err(_) => return Err(error()),
    };
    if result { Ok(()) } else { Err(error()) }
}

struct Stream {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    queue: DispatchRetained<DispatchQueue>,
    closed: bool,
}
impl Stream {
    fn open(
        display: &SCDisplay,
        g: Geometry,
        slot: Arc<Slot>,
        budget: Arc<Semaphore>,
    ) -> Result<Self, DesktopError> {
        let this = Output::alloc().set_ivars(OutputState {
            slot,
            geometry: g,
            budget,
        });
        // SAFETY: NSObject init has the documented signature and initializes
        // the Rust subclass's already-installed thread-safe ivars.
        let output: Retained<Output> = unsafe { msg_send![super(this), init] };
        // SAFETY: Newly allocated objects are used exclusively on this worker.
        // A display-wide filter excludes no windows; audio is never requested.
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(g.width as usize);
            config.setHeight(g.height as usize);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setQueueDepth(3);
            config.setShowsCursor(true);
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        let queue = DispatchQueue::new("ai.nddev.remote.screen", None);
        // SAFETY: A serial sample queue invokes only the thread-safe Output
        // subclass; the output and queue are retained through stop and drain.
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|_| error())?;
        let owned = Self {
            stream,
            output,
            queue,
            closed: false,
        };
        completed(&owned.stream, true)?;
        Ok(owned)
    }
    fn close(&mut self) -> Result<(), DesktopError> {
        if self.closed {
            return Ok(());
        }
        self.output.ivars().slot.fail();
        let stopped = completed(&self.stream, false);
        // SAFETY: Stop completion was observed. Detach and drain the serial
        // queue before releasing the subclass's callback state. Even a native
        // stop error is propagated after callbacks have actually drained.
        let detached = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                SCStreamOutputType::Screen,
            )
        }
        .map_err(|_| error());
        self.queue.exec_sync(|| {});
        self.closed = true;
        stopped.and(detached)
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            tracing::warn!(%error, "ScreenCaptureKit stop failed");
        }
    }
}

pub(crate) struct Prepared {
    pub geometries: Vec<Geometry>,
    pub slots: Vec<Arc<Slot>>,
    pub input: Arc<crate::input::macos::Group>,
}

pub(crate) fn run(
    stop: &Arc<AtomicBool>,
    ready: tokio::sync::oneshot::Sender<Result<Prepared, DesktopError>>,
) -> Result<(), DesktopError> {
    autoreleasepool(|_| {
        if !crate::input::macos::available() {
            return Err(error());
        }
        let input = crate::input::macos::Group::new(stop.clone());
        struct InputClose(Arc<crate::input::macos::Group>);
        impl Drop for InputClose {
            fn drop(&mut self) {
                self.0.stop();
            }
        }
        let _input_close = InputClose(input.clone());
        let snapshot = inventory()?;
        // SAFETY: Reading the retained immutable snapshot on its owner worker.
        let displays = unsafe { snapshot.0.displays() };
        if displays.count() == 0 || displays.count() > 16 {
            return Err(error());
        }
        let budget = Arc::new(Semaphore::new(256 * 1024 * 1024));
        let mut geometries = Vec::new();
        let mut slots = Vec::new();
        let mut streams = Vec::new();
        let mut total = 0usize;
        for display in displays.iter() {
            if stop.load(Ordering::Acquire) {
                return Err(error());
            }
            // SAFETY: Each SCDisplay belongs to the retained immutable snapshot.
            let g = geometry(unsafe { display.displayID() })?;
            total += crate::frame_bytes(g.width as usize, g.height as usize).ok_or_else(error)?;
            if total > 128 * 1024 * 1024
                || geometries.iter().any(|other: &Geometry| other.id == g.id)
            {
                return Err(error());
            }
            let slot = Slot::new(g.id, Some((g.width, g.height)));
            streams.push(Stream::open(&display, g, slot.clone(), budget.clone())?);
            geometries.push(g);
            slots.push(slot);
        }
        if ready
            .send(Ok(Prepared {
                geometries: geometries.clone(),
                slots: slots.clone(),
                input: input.clone(),
            }))
            .is_err()
        {
            return Ok(());
        }
        while !stop.load(Ordering::Acquire) {
            if !CGPreflightScreenCaptureAccess()
                || !crate::input::macos::available()
                || slots.iter().any(|s| s.extent().is_err())
                || geometries.iter().any(|g| geometry(g.id).ok() != Some(*g))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Release);
        input.stop();
        for slot in &slots {
            slot.fail();
        }
        let mut result = Ok(());
        for stream in streams.iter_mut().rev() {
            let closed = stream.close();
            if result.is_ok() {
                result = closed;
            }
        }
        result
    })
}
