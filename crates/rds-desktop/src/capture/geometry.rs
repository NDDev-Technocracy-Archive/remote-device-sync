//! Native pixel coordinates and global logical points are different on Retina.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub x: f64,
    pub y: f64,
    pub logical_width: f64,
    pub logical_height: f64,
    pub primary: bool,
}

impl Geometry {
    pub fn point(self, x: f64, y: f64) -> Option<(f64, f64)> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.width)
            || y >= f64::from(self.height)
            || self.width == 0
            || self.height == 0
            || !self.logical_width.is_finite()
            || !self.logical_height.is_finite()
            || self.logical_width <= 0.0
            || self.logical_height <= 0.0
            || !self.x.is_finite()
            || !self.y.is_finite()
        {
            return None;
        }
        Some((
            self.x + x * self.logical_width / f64::from(self.width),
            self.y + y * self.logical_height / f64::from(self.height),
        ))
    }

    pub fn info(self) -> rds_core::DisplayInfo {
        rds_core::DisplayInfo {
            // The primary-screen slot matches Linux and existing viewer
            // defaults. Its prepared owner still retains/checks the actual
            // native ID; secondary screens keep their distinct native IDs.
            index: if self.primary { 0 } else { self.id },
            width: self.width,
            height: self.height,
            primary: self.primary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retina_pixels_map_into_the_selected_negative_origin_monitor() {
        let g = Geometry {
            id: 4,
            width: 2880,
            height: 1800,
            x: -1440.0,
            y: -200.0,
            logical_width: 1440.0,
            logical_height: 900.0,
            primary: false,
        };
        assert_eq!(g.point(0.0, 0.0), Some((-1440.0, -200.0)));
        assert_eq!(g.point(1440.0, 900.0), Some((-720.0, 250.0)));
        for (x, y) in [
            (2880.0, 0.0),
            (-0.5, 0.0),
            (0.0, 1800.0),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(g.point(x, y).is_none());
        }
        assert_eq!(g.info().index, 4);
        assert_eq!(Geometry { primary: true, ..g }.info().index, 0);
    }
}
