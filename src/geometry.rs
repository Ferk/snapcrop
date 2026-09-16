#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    #[cfg(test)]
    pub fn from_points(a: Point, b: Point, bounds_width: u32, bounds_height: u32) -> Option<Self> {
        let max_x = i32::try_from(bounds_width).unwrap_or(i32::MAX);
        let max_y = i32::try_from(bounds_height).unwrap_or(i32::MAX);
        let ax = a.x.clamp(0, max_x);
        let ay = a.y.clamp(0, max_y);
        let bx = b.x.clamp(0, max_x);
        let by = b.y.clamp(0, max_y);
        let x = ax.min(bx) as u32;
        let y = ay.min(by) as u32;
        let width = ax.abs_diff(bx);
        let height = ay.abs_diff(by);

        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn to_buffer(
        self,
        logical_width: u32,
        logical_height: u32,
        buffer_width: u32,
        buffer_height: u32,
    ) -> Self {
        fn edge(value: u32, logical: u32, buffer: u32) -> u32 {
            if logical == 0 {
                0
            } else {
                ((u64::from(value) * u64::from(buffer)) / u64::from(logical)) as u32
            }
        }

        let x1 = edge(self.x, logical_width, buffer_width).min(buffer_width);
        let y1 = edge(self.y, logical_height, buffer_height).min(buffer_height);
        let x2 = edge(self.x + self.width, logical_width, buffer_width).min(buffer_width);
        let y2 = edge(self.y + self.height, logical_height, buffer_height).min(buffer_height);
        Self {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_reverse_drag() {
        assert_eq!(
            Rect::from_points(Point { x: 90, y: 60 }, Point { x: 10, y: 20 }, 100, 100),
            Some(Rect {
                x: 10,
                y: 20,
                width: 80,
                height: 40
            })
        );
    }

    #[test]
    fn clamps_to_output_and_rejects_empty() {
        assert_eq!(
            Rect::from_points(Point { x: -10, y: 20 }, Point { x: 120, y: 80 }, 100, 60),
            Some(Rect {
                x: 0,
                y: 20,
                width: 100,
                height: 40
            })
        );
        assert_eq!(
            Rect::from_points(Point { x: 3, y: 3 }, Point { x: 3, y: 9 }, 10, 10),
            None
        );
    }

    #[test]
    fn maps_logical_edges_to_buffer_pixels() {
        let logical = Rect {
            x: 3,
            y: 2,
            width: 5,
            height: 4,
        };
        assert_eq!(
            logical.to_buffer(10, 10, 20, 30),
            Rect {
                x: 6,
                y: 6,
                width: 10,
                height: 12
            }
        );
    }
}
