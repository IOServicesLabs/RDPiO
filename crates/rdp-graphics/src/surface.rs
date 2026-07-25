//! EGFX surface bookkeeping (MS-RDPEGFX): tracks the surfaces the server
//! creates and where each is mapped on the output, so decoded surface updates
//! can be composited at the right desktop coordinates.
//!
//! A surface is an off-screen bitmap the server draws into (via wire-to-surface,
//! solid-fill, surface-to-surface, etc.). `MAP_SURFACE_TO_OUTPUT` binds a
//! surface's top-left to a point on the desktop; an update at surface rect
//! `(l,t,r,b)` therefore lands at `(outputX + l, outputY + t)` on screen.

use std::collections::HashMap;

use rdp_pdu::gfx::GfxCommand;

/// A server-created EGFX surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub width: u16,
    pub height: u16,
    pub pixel_format: u8,
    /// Desktop origin this surface is mapped to, once `MapSurfaceToOutput`
    /// arrives. `None` until then.
    pub output: Option<(u32, u32)>,
}

/// Tracks live surfaces and their output mappings for one connection.
#[derive(Default)]
pub struct SurfaceTable {
    surfaces: HashMap<u16, Surface>,
}

impl SurfaceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the table from a graphics command. Only create/delete/map affect
    /// surface state; other commands are ignored.
    pub fn apply(&mut self, command: &GfxCommand) {
        match command {
            GfxCommand::CreateSurface {
                surface_id,
                width,
                height,
                pixel_format,
            } => {
                self.surfaces.insert(
                    *surface_id,
                    Surface {
                        width: *width,
                        height: *height,
                        pixel_format: *pixel_format,
                        output: None,
                    },
                );
            }
            GfxCommand::DeleteSurface { surface_id } => {
                self.surfaces.remove(surface_id);
            }
            GfxCommand::MapSurfaceToOutput { surface_id, x, y } => {
                if let Some(s) = self.surfaces.get_mut(surface_id) {
                    s.output = Some((*x, *y));
                }
            }
            _ => {}
        }
    }

    /// The surface, if it exists.
    pub fn get(&self, surface_id: u16) -> Option<&Surface> {
        self.surfaces.get(&surface_id)
    }

    /// The desktop origin a surface is mapped to. A created-but-unmapped surface
    /// reports `(0, 0)`; an unknown surface reports `None`.
    pub fn output_origin(&self, surface_id: u16) -> Option<(u32, u32)> {
        self.surfaces
            .get(&surface_id)
            .map(|s| s.output.unwrap_or((0, 0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(id: u16, w: u16, h: u16) -> GfxCommand {
        GfxCommand::CreateSurface {
            surface_id: id,
            width: w,
            height: h,
            pixel_format: 0x20,
        }
    }

    #[test]
    fn create_map_lookup() {
        let mut t = SurfaceTable::new();
        t.apply(&create(1, 1024, 768));
        // Created but unmapped → origin (0,0).
        assert_eq!(t.output_origin(1), Some((0, 0)));
        t.apply(&GfxCommand::MapSurfaceToOutput {
            surface_id: 1,
            x: 100,
            y: 50,
        });
        assert_eq!(t.output_origin(1), Some((100, 50)));
        assert_eq!(t.get(1).unwrap().width, 1024);
    }

    #[test]
    fn delete_removes() {
        let mut t = SurfaceTable::new();
        t.apply(&create(2, 64, 64));
        t.apply(&GfxCommand::DeleteSurface { surface_id: 2 });
        assert_eq!(t.output_origin(2), None);
    }

    #[test]
    fn unknown_surface_is_none() {
        let t = SurfaceTable::new();
        assert_eq!(t.output_origin(9), None);
    }

    #[test]
    fn map_unknown_surface_is_ignored() {
        let mut t = SurfaceTable::new();
        t.apply(&GfxCommand::MapSurfaceToOutput {
            surface_id: 5,
            x: 1,
            y: 2,
        });
        assert_eq!(t.get(5), None);
    }
}
