//! Geometry tracking (MS-RDPEGT) — *where* a region of the remote desktop is.
//!
//! The server opens `Microsoft::Windows::RDS::Geometry::v08.01` and streams one
//! `MAPPED_GEOMETRY_PACKET` per change, keyed by a 64-bit **mapping id**: the
//! desktop rectangle of the tracked region and of its top-level window, plus
//! the region's visible part. Redirected media is drawn at a mapping's geometry
//! — Teams' WebRTC redirector hands the client a video element's mapping id in
//! `MediaElement.notifyVisibilityChanged`.
//!
//! Wire layout (little-endian): `cbGeometryData`(4) `Version`(4) `MappingId`(8)
//! `UpdateType`(4) `Flags`(4); then, for an update: `TopLevelId`(8), the region's
//! `Left/Top/Right/Bottom` (4 each, relative to the top-level window), the
//! top-level window's `Left/Top/Right/Bottom` (4 each, desktop coordinates),
//! `GeometryType`(4) `cbGeometryBuffer`(4) and an optional `RGNDATA` visible
//! region (32-byte header then `nCount` rectangles).

/// `UpdateType`: the mapping's geometry changed.
const GEOMETRY_UPDATE: u32 = 0x1;
/// `UpdateType`: the mapping is gone.
const GEOMETRY_CLEAR: u32 = 0x2;
/// Bytes before the update-only fields.
const HEADER_LEN: usize = 24;
/// The fixed update fields after the header, up to (not including) the region.
const UPDATE_LEN: usize = 8 + 16 + 16 + 4 + 4;
/// `RGNDATAHEADER` size.
const RGN_HEADER_LEN: usize = 32;

/// A rectangle as `{left, top, right, bottom}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeoRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl GeoRect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }
    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }
}

/// One mapping's current geometry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappedGeometry {
    pub mapping_id: u64,
    /// The top-level window (`HWND`) the region belongs to.
    pub top_level_id: u64,
    /// The region, relative to the top-level window.
    pub rect: GeoRect,
    /// The top-level window, in desktop coordinates.
    pub top_level: GeoRect,
    /// The region's visible part (`RGNDATA` rectangles); empty when unsent.
    pub visible: Vec<GeoRect>,
}

impl MappedGeometry {
    /// The region in desktop coordinates: the top-level origin plus the
    /// window-relative rectangle.
    pub fn desktop_rect(&self) -> GeoRect {
        GeoRect {
            left: self.top_level.left + self.rect.left,
            top: self.top_level.top + self.rect.top,
            right: self.top_level.left + self.rect.right,
            bottom: self.top_level.top + self.rect.bottom,
        }
    }
}

/// One decoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeometryUpdate {
    Update(MappedGeometry),
    Clear(u64),
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn rect_at(b: &[u8], at: usize) -> Option<GeoRect> {
    Some(GeoRect {
        left: i32_at(b, at)?,
        top: i32_at(b, at + 4)?,
        right: i32_at(b, at + 8)?,
        bottom: i32_at(b, at + 12)?,
    })
}

/// Decode one `MAPPED_GEOMETRY_PACKET`. `None` for a truncated packet or an
/// unknown update type.
pub fn parse(pdu: &[u8]) -> Option<GeometryUpdate> {
    let mapping_id = u64_at(pdu, 8)?;
    match u32_at(pdu, 16)? {
        GEOMETRY_CLEAR => Some(GeometryUpdate::Clear(mapping_id)),
        GEOMETRY_UPDATE => {
            let at = HEADER_LEN;
            let top_level_id = u64_at(pdu, at)?;
            let rect = rect_at(pdu, at + 8)?;
            let top_level = rect_at(pdu, at + 24)?;
            let cb_region = u32_at(pdu, at + 44)? as usize;
            let region = pdu.get(at + UPDATE_LEN..at + UPDATE_LEN + cb_region)?;
            Some(GeometryUpdate::Update(MappedGeometry {
                mapping_id,
                top_level_id,
                rect,
                top_level,
                visible: parse_region(region),
            }))
        }
        _ => None,
    }
}

/// The rectangles of an `RGNDATA` (empty if absent or malformed).
fn parse_region(b: &[u8]) -> Vec<GeoRect> {
    let Some(count) = u32_at(b, 8) else {
        return Vec::new();
    };
    (0..count as usize)
        .map_while(|i| rect_at(b, RGN_HEADER_LEN + i * 16))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(out: &mut Vec<u8>, l: i32, t: i32, r: i32, b: i32) {
        for v in [l, t, r, b] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn update_packet(mapping: u64, region: &[(i32, i32, i32, i32)]) -> Vec<u8> {
        let mut rgn = Vec::new();
        if !region.is_empty() {
            rgn.extend_from_slice(&32u32.to_le_bytes()); // dwSize
            rgn.extend_from_slice(&1u32.to_le_bytes()); // RDH_RECTANGLES
            rgn.extend_from_slice(&(region.len() as u32).to_le_bytes());
            rgn.extend_from_slice(&((region.len() * 16) as u32).to_le_bytes());
            rect(&mut rgn, 0, 0, 640, 360); // bounds
            for &(l, t, r, b) in region {
                rect(&mut rgn, l, t, r, b);
            }
        }
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // Version
        body.extend_from_slice(&mapping.to_le_bytes());
        body.extend_from_slice(&GEOMETRY_UPDATE.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // Flags
        body.extend_from_slice(&0x3_0358u64.to_le_bytes()); // TopLevelId
        rect(&mut body, 100, 200, 740, 560); // region, window-relative
        rect(&mut body, 50, 40, 1850, 1040); // top-level, desktop
        body.extend_from_slice(&2u32.to_le_bytes()); // RDH_RGN
        body.extend_from_slice(&(rgn.len() as u32).to_le_bytes());
        body.extend_from_slice(&rgn);
        let mut pdu = ((body.len() + 4) as u32).to_le_bytes().to_vec();
        pdu.extend_from_slice(&body);
        pdu
    }

    #[test]
    fn parses_an_update_with_its_visible_region() {
        let id = 0x8000_27c7_0035_0d14;
        let pdu = update_packet(id, &[(0, 0, 640, 300), (0, 300, 500, 360)]);
        let Some(GeometryUpdate::Update(g)) = parse(&pdu) else {
            panic!("not an update");
        };
        assert_eq!(g.mapping_id, id);
        assert_eq!(g.top_level_id, 0x3_0358);
        assert_eq!((g.rect.width(), g.rect.height()), (640, 360));
        assert_eq!(
            g.desktop_rect(),
            GeoRect { left: 150, top: 240, right: 790, bottom: 600 }
        );
        assert_eq!(g.visible.len(), 2);
        assert_eq!(g.visible[1], GeoRect { left: 0, top: 300, right: 500, bottom: 360 });
    }

    #[test]
    fn parses_an_update_without_a_region() {
        let Some(GeometryUpdate::Update(g)) = parse(&update_packet(7, &[])) else {
            panic!("not an update");
        };
        assert!(g.visible.is_empty());
    }

    #[test]
    fn parses_a_clear_and_rejects_truncation() {
        let mut pdu = 24u32.to_le_bytes().to_vec();
        pdu.extend_from_slice(&1u32.to_le_bytes());
        pdu.extend_from_slice(&42u64.to_le_bytes());
        pdu.extend_from_slice(&GEOMETRY_CLEAR.to_le_bytes());
        pdu.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse(&pdu), Some(GeometryUpdate::Clear(42)));
        let update = update_packet(9, &[]);
        assert_eq!(parse(&update[..40]), None);
    }
}
