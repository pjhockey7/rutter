//! Mapbox Vector Tile decoding.
//!
//! An MVT tile is a protobuf message. Rather than take a protobuf library
//! and a build-time code generator, this reads the wire format directly —
//! the MVT schema is eight messages, frozen since 2016, and the whole
//! decoder is shorter than the generated code would be. It also keeps this
//! module dependency-free, which means it is testable without a database, a
//! GPU or a network.
//!
//! The format, briefly. A tile holds layers; a layer holds features and two
//! string-interning tables; a feature holds a geometry and indices into
//! those tables. Coordinates are integers in a tile-local grid — normally
//! 4096×4096 — with x running east and **y running south**, which matters
//! because it is the opposite of every geographic convention and the same as
//! every screen convention.
//!
//! Geometry is a flat list of integers interpreted as a tiny instruction
//! stream: a command integer saying "move/line/close, N times", followed by
//! 2N zigzag-encoded deltas from a running cursor. Deltas, not absolutes, is
//! why a coastline costs a few bytes per point.

/// What went wrong decoding a tile.
///
/// Every variant means the input is not a valid tile, and all of them are
/// reachable from a corrupt or truncated file — which is the normal failure
/// for chart data copied onto a phone over a flaky cable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Ran off the end of the buffer.
    Truncated,
    /// A varint longer than 10 bytes, which cannot be a u64.
    VarintTooLong,
    /// A field was encoded with a wire type its type cannot use.
    WireType { field: u32, wire: u8 },
    /// A wire type this decoder does not know. Protobuf reserves 3, 4 and 6.
    UnknownWireType(u8),
    /// A string field was not UTF-8.
    NotUtf8,
    /// The geometry instruction stream does not make sense.
    Geometry(&'static str),
    /// A tag index pointed past the end of the layer's key or value table.
    TagOutOfRange,
    /// Only version 1 and 2 layers are understood.
    Version(u32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "tile data ends mid-field"),
            Self::VarintTooLong => write!(f, "varint longer than 64 bits"),
            Self::WireType { field, wire } => {
                write!(f, "field {field} has wire type {wire}, which it cannot use")
            }
            Self::UnknownWireType(w) => write!(f, "unknown protobuf wire type {w}"),
            Self::NotUtf8 => write!(f, "string field is not UTF-8"),
            Self::Geometry(why) => write!(f, "bad geometry: {why}"),
            Self::TagOutOfRange => write!(f, "attribute tag points outside the layer tables"),
            Self::Version(v) => write!(f, "unsupported vector tile version {v}"),
        }
    }
}

impl std::error::Error for Error {}

// ── protobuf wire format ─────────────────────────────────────────────────────

/// Wire types. 3 and 4 (the deprecated groups) and 6 are not produced by any
/// MVT writer, so they are rejected rather than skipped.
const WIRE_VARINT: u8 = 0;
const WIRE_64BIT: u8 = 1;
const WIRE_BYTES: u8 = 2;
const WIRE_32BIT: u8 = 5;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        for shift in 0..10 {
            let b = self.byte()?;
            value |= u64::from(b & 0x7f) << (shift * 7);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::VarintTooLong)
    }

    /// Field number and wire type of the next field.
    fn field(&mut self) -> Result<(u32, u8), Error> {
        let key = self.varint()?;
        Ok(((key >> 3) as u32, (key & 0x7) as u8))
    }

    /// A length-delimited field's payload.
    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).ok_or(Error::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn string(&mut self) -> Result<String, Error> {
        let b = self.bytes()?;
        core::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| Error::NotUtf8)
    }

    fn fixed32(&mut self) -> Result<u32, Error> {
        let mut v = [0u8; 4];
        for b in &mut v {
            *b = self.byte()?;
        }
        Ok(u32::from_le_bytes(v))
    }

    fn fixed64(&mut self) -> Result<u64, Error> {
        let mut v = [0u8; 8];
        for b in &mut v {
            *b = self.byte()?;
        }
        Ok(u64::from_le_bytes(v))
    }

    /// Step over a field this decoder does not care about.
    ///
    /// Not optional politeness: tippecanoe and other writers emit fields
    /// outside the published schema, and a decoder that treats an unknown
    /// field as an error rejects real tiles.
    fn skip(&mut self, wire: u8) -> Result<(), Error> {
        match wire {
            WIRE_VARINT => {
                self.varint()?;
            }
            WIRE_64BIT => {
                self.fixed64()?;
            }
            WIRE_BYTES => {
                self.bytes()?;
            }
            WIRE_32BIT => {
                self.fixed32()?;
            }
            other => return Err(Error::UnknownWireType(other)),
        }
        Ok(())
    }

    /// Read a `packed repeated uint32` field, which is a length-delimited
    /// run of varints.
    fn packed_u32(&mut self) -> Result<Vec<u32>, Error> {
        let payload = self.bytes()?;
        let mut inner = Reader::new(payload);
        let mut out = Vec::new();
        while !inner.done() {
            out.push(inner.varint()? as u32);
        }
        Ok(out)
    }
}

// ── the tile ─────────────────────────────────────────────────────────────────

/// A decoded vector tile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tile {
    pub layers: Vec<Layer>,
}

/// One named layer: a group of features sharing an attribute schema.
///
/// For chart data from an ENC these correspond to S-57 object classes —
/// COALNE for coastline, DEPARE for depth areas, SOUNDG for soundings —
/// which is how a renderer decides what a feature should look like.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    pub version: u32,
    /// Side length of the tile-local coordinate grid. 4096 unless the writer
    /// says otherwise.
    pub extent: u32,
    pub features: Vec<Feature>,
}

/// One feature: a geometry plus its attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub id: Option<u64>,
    pub geometry: Geometry,
    /// Already resolved against the layer's key and value tables, because a
    /// caller holding indices into tables it does not have is no use.
    pub attributes: Vec<(String, Value)>,
}

impl Feature {
    /// Look up one attribute by name.
    pub fn attribute(&self, name: &str) -> Option<&Value> {
        self.attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

/// An attribute value. Protobuf's seven numeric cases collapse to three,
/// because no consumer here cares whether a depth arrived as a float or a
/// double.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Float(f64),
    Int(i64),
    Uint(u64),
    Bool(bool),
}

impl Value {
    /// The value as a number, whatever integer or float case it arrived in.
    ///
    /// Chart attributes are overwhelmingly numeric — depths, heights,
    /// ranges — and which protobuf case a writer chose for `5` is not
    /// something a renderer should have to branch on.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Int(i) => Some(*i as f64),
            Self::Uint(u) => Some(*u as f64),
            Self::Bool(_) | Self::String(_) => None,
        }
    }
}

/// Tile-local geometry, in integer grid coordinates with y running south.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Points(Vec<[i32; 2]>),
    /// Each entry is one connected run of points.
    Lines(Vec<Vec<[i32; 2]>>),
    /// Each entry is one polygon: ring 0 is its exterior, the rest are holes.
    Polygons(Vec<Polygon>),
    /// Geometry type 0, which the spec allows and nothing emits.
    Unknown,
}

/// A polygon as an exterior ring plus holes. Rings are closed — the last
/// point repeats the first.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub exterior: Vec<[i32; 2]>,
    pub holes: Vec<Vec<[i32; 2]>>,
}

impl Polygon {
    /// Every ring, exterior first. What a renderer drawing outlines wants.
    pub fn rings(&self) -> impl Iterator<Item = &Vec<[i32; 2]>> {
        core::iter::once(&self.exterior).chain(self.holes.iter())
    }
}

impl Tile {
    /// Decode a tile from uncompressed protobuf bytes.
    ///
    /// The caller is responsible for gunzipping; MBTiles stores these
    /// compressed but the format itself says nothing about that.
    pub fn decode(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(data);
        let mut layers = Vec::new();

        while !r.done() {
            let (field, wire) = r.field()?;
            match (field, wire) {
                // Tile.layers = 3
                (3, WIRE_BYTES) => layers.push(Layer::decode(r.bytes()?)?),
                (3, _) => return Err(Error::WireType { field, wire }),
                _ => r.skip(wire)?,
            }
        }

        Ok(Self { layers })
    }

    /// Find a layer by name.
    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.name == name)
    }
}

impl Layer {
    fn decode(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(data);

        let mut name = String::new();
        let mut version = 1u32;
        let mut extent = 4096u32;
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        // Features are kept encoded until the tables are read: the spec does
        // not fix field order, and a feature cannot resolve its attributes
        // before `keys` and `values` exist.
        let mut raw_features: Vec<&[u8]> = Vec::new();

        while !r.done() {
            let (field, wire) = r.field()?;
            match (field, wire) {
                (1, WIRE_BYTES) => name = r.string()?,
                (2, WIRE_BYTES) => raw_features.push(r.bytes()?),
                (3, WIRE_BYTES) => keys.push(r.string()?),
                (4, WIRE_BYTES) => values.push(decode_value(r.bytes()?)?),
                (5, WIRE_VARINT) => extent = r.varint()? as u32,
                (15, WIRE_VARINT) => version = r.varint()? as u32,
                (1 | 2 | 3 | 4 | 5 | 15, _) => return Err(Error::WireType { field, wire }),
                _ => r.skip(wire)?,
            }
        }

        if version != 1 && version != 2 {
            return Err(Error::Version(version));
        }
        if extent == 0 {
            return Err(Error::Geometry("layer extent is zero"));
        }

        let features = raw_features
            .into_iter()
            .map(|f| Feature::decode(f, &keys, &values))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            name,
            version,
            extent,
            features,
        })
    }
}

fn decode_value(data: &[u8]) -> Result<Value, Error> {
    let mut r = Reader::new(data);
    // A Value is a one-of, but it is encoded as seven optional fields; take
    // the last one present, which is what every other decoder does.
    let mut out = Value::Bool(false);
    let mut seen = false;

    while !r.done() {
        let (field, wire) = r.field()?;
        out = match (field, wire) {
            (1, WIRE_BYTES) => Value::String(r.string()?),
            (2, WIRE_32BIT) => Value::Float(f32::from_bits(r.fixed32()?) as f64),
            (3, WIRE_64BIT) => Value::Float(f64::from_bits(r.fixed64()?)),
            (4, WIRE_VARINT) => Value::Int(r.varint()? as i64),
            (5, WIRE_VARINT) => Value::Uint(r.varint()?),
            (6, WIRE_VARINT) => Value::Int(zigzag64(r.varint()?)),
            (7, WIRE_VARINT) => Value::Bool(r.varint()? != 0),
            (1..=7, _) => return Err(Error::WireType { field, wire }),
            _ => {
                r.skip(wire)?;
                continue;
            }
        };
        seen = true;
    }

    if seen {
        Ok(out)
    } else {
        // An empty Value is legal protobuf and meaningless here; an empty
        // string is the least surprising reading.
        Ok(Value::String(String::new()))
    }
}

impl Feature {
    fn decode(data: &[u8], keys: &[String], values: &[Value]) -> Result<Self, Error> {
        let mut r = Reader::new(data);

        let mut id = None;
        let mut tags: Vec<u32> = Vec::new();
        let mut kind = 0u32;
        let mut commands: Vec<u32> = Vec::new();

        while !r.done() {
            let (field, wire) = r.field()?;
            match (field, wire) {
                (1, WIRE_VARINT) => id = Some(r.varint()?),
                (2, WIRE_BYTES) => tags = r.packed_u32()?,
                // A single-element packed field may be written unpacked.
                (2, WIRE_VARINT) => tags.push(r.varint()? as u32),
                (3, WIRE_VARINT) => kind = r.varint()? as u32,
                (4, WIRE_BYTES) => commands = r.packed_u32()?,
                (4, WIRE_VARINT) => commands.push(r.varint()? as u32),
                (1..=4, _) => return Err(Error::WireType { field, wire }),
                _ => r.skip(wire)?,
            }
        }

        if tags.len() % 2 != 0 {
            return Err(Error::Geometry("odd number of attribute tags"));
        }

        let mut attributes = Vec::with_capacity(tags.len() / 2);
        for pair in tags.chunks_exact(2) {
            let key = keys.get(pair[0] as usize).ok_or(Error::TagOutOfRange)?;
            let value = values.get(pair[1] as usize).ok_or(Error::TagOutOfRange)?;
            attributes.push((key.clone(), value.clone()));
        }

        Ok(Self {
            id,
            geometry: decode_geometry(&commands, kind)?,
            attributes,
        })
    }
}

// ── geometry ─────────────────────────────────────────────────────────────────

const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

fn zigzag32(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

fn zigzag64(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// Walk the command stream into runs of points.
///
/// Returns one run per MoveTo. Whether a run is a point, a line or a ring is
/// decided by the feature's declared type, not by the stream — the same
/// encoding serves all three.
fn decode_runs(commands: &[u32]) -> Result<Vec<Vec<[i32; 2]>>, Error> {
    let mut runs: Vec<Vec<[i32; 2]>> = Vec::new();
    let mut cursor = [0i32; 2];
    let mut i = 0;

    while i < commands.len() {
        let command = commands[i];
        let id = command & 0x7;
        let count = (command >> 3) as usize;
        i += 1;

        match id {
            CMD_MOVE_TO | CMD_LINE_TO => {
                if count == 0 {
                    return Err(Error::Geometry("move or line with zero points"));
                }
                // Two parameters per point, and running past the end means
                // the stream was truncated.
                let needed = count
                    .checked_mul(2)
                    .ok_or(Error::Geometry("count overflow"))?;
                let params = commands
                    .get(i..i + needed)
                    .ok_or(Error::Geometry("command stream ends mid-point"))?;
                i += needed;

                if id == CMD_MOVE_TO {
                    // Each MoveTo starts a fresh run. For a multipoint this
                    // is one MoveTo with many points, which still belongs in
                    // one run.
                    runs.push(Vec::with_capacity(count));
                } else if runs.is_empty() {
                    return Err(Error::Geometry("line before any move"));
                }

                // `runs` is non-empty: either just pushed, or checked above.
                let run = match runs.last_mut() {
                    Some(r) => r,
                    None => return Err(Error::Geometry("line before any move")),
                };

                for point in params.chunks_exact(2) {
                    cursor[0] = cursor[0].wrapping_add(zigzag32(point[0]));
                    cursor[1] = cursor[1].wrapping_add(zigzag32(point[1]));
                    run.push(cursor);
                }
            }

            CMD_CLOSE_PATH => {
                if count != 1 {
                    return Err(Error::Geometry("ClosePath must have count 1"));
                }
                let run = runs
                    .last_mut()
                    .ok_or(Error::Geometry("close before any move"))?;
                // Close explicitly, so a ring's last point equals its first
                // and consumers need no special case. The cursor does not
                // move: a ClosePath draws back to the ring's start without
                // changing where the next command begins from.
                if let Some(&first) = run.first() {
                    if run.last() != Some(&first) {
                        run.push(first);
                    }
                }
            }

            other => {
                return Err(if other == 0 {
                    Error::Geometry("command id 0 is not defined")
                } else {
                    Error::Geometry("unknown geometry command")
                })
            }
        }
    }

    Ok(runs)
}

fn decode_geometry(commands: &[u32], kind: u32) -> Result<Geometry, Error> {
    if commands.is_empty() {
        // A feature with no geometry is legal and useless; report it as the
        // matching empty rather than an error.
        return Ok(match kind {
            1 => Geometry::Points(Vec::new()),
            2 => Geometry::Lines(Vec::new()),
            3 => Geometry::Polygons(Vec::new()),
            _ => Geometry::Unknown,
        });
    }

    let runs = decode_runs(commands)?;

    Ok(match kind {
        1 => Geometry::Points(runs.into_iter().flatten().collect()),
        2 => Geometry::Lines(runs),
        3 => Geometry::Polygons(group_rings(runs)),
        _ => Geometry::Unknown,
    })
}

/// Twice the signed area of a ring, by the shoelace formula.
///
/// Kept as `i64` and un-halved so it is exact: these are integer grid
/// coordinates, and a ring whose true area is a half-unit should not round
/// to zero and be mistaken for degenerate.
fn signed_area_x2(ring: &[[i32; 2]]) -> i64 {
    let mut acc = 0i64;
    for pair in ring.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        acc += i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1]);
    }
    acc
}

/// Group a polygon feature's rings into polygons.
///
/// MVT decides exterior from interior by winding: in tile coordinates, which
/// run y-south, an exterior ring is clockwise and that gives a positive
/// shoelace sum. Each exterior ring starts a new polygon and the interior
/// rings that follow are its holes.
///
/// Rings with zero area are dropped. They appear in real tiles when
/// simplification collapses a small island, and they are neither an exterior
/// nor an interior ring.
fn group_rings(runs: Vec<Vec<[i32; 2]>>) -> Vec<Polygon> {
    let mut polygons: Vec<Polygon> = Vec::new();

    for ring in runs {
        if ring.len() < 4 {
            // Fewer than four points cannot close a triangle.
            continue;
        }
        let area = signed_area_x2(&ring);
        if area > 0 {
            polygons.push(Polygon {
                exterior: ring,
                holes: Vec::new(),
            });
        } else if area < 0 {
            match polygons.last_mut() {
                Some(p) => p.holes.push(ring),
                // A hole before any exterior ring is malformed. Promote it
                // rather than discard it: dropping chart geometry silently
                // is worse than drawing a ring with the wrong winding.
                None => polygons.push(Polygon {
                    exterior: ring,
                    holes: Vec::new(),
                }),
            }
        }
    }

    polygons
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a protobuf varint, for building test fixtures by hand.
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn key(field: u32, wire: u8) -> u64 {
        u64::from(field) << 3 | u64::from(wire)
    }

    /// A length-delimited field.
    fn delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(key(field, WIRE_BYTES), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn varint_field(field: u32, value: u64, out: &mut Vec<u8>) {
        varint(key(field, WIRE_VARINT), out);
        varint(value, out);
    }

    fn packed(field: u32, values: &[u32], out: &mut Vec<u8>) {
        let mut payload = Vec::new();
        for &v in values {
            varint(u64::from(v), &mut payload);
        }
        delimited(field, &payload, out);
    }

    fn zz(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    fn command(id: u32, count: u32) -> u32 {
        id | (count << 3)
    }

    /// Build a one-layer, one-feature tile.
    fn tile_with(kind: u32, commands: &[u32], extent: u32) -> Vec<u8> {
        let mut feature = Vec::new();
        varint_field(3, u64::from(kind), &mut feature);
        packed(4, commands, &mut feature);

        let mut layer = Vec::new();
        delimited(1, b"chart", &mut layer);
        delimited(2, &feature, &mut layer);
        varint_field(5, u64::from(extent), &mut layer);
        varint_field(15, 2, &mut layer);

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);
        tile
    }

    // ── wire format ──────────────────────────────────────────────────────

    #[test]
    fn varints_round_trip() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            varint(value, &mut buf);
            assert_eq!(Reader::new(&buf).varint(), Ok(value), "value {value}");
        }
    }

    #[test]
    fn truncated_varint_is_an_error() {
        // 0x80 sets the continuation bit with nothing following.
        assert_eq!(Reader::new(&[0x80]).varint(), Err(Error::Truncated));
    }

    #[test]
    fn overlong_varint_is_rejected() {
        let buf = [0x80u8; 11];
        assert_eq!(Reader::new(&buf).varint(), Err(Error::VarintTooLong));
    }

    #[test]
    fn truncated_length_prefix_is_an_error() {
        // Claims 20 bytes, supplies 2.
        assert_eq!(Reader::new(&[20, 1, 2]).bytes(), Err(Error::Truncated));
    }

    #[test]
    fn zigzag_matches_the_spec() {
        assert_eq!(zigzag32(0), 0);
        assert_eq!(zigzag32(1), -1);
        assert_eq!(zigzag32(2), 1);
        assert_eq!(zigzag32(3), -2);
        assert_eq!(zigzag32(4294967294), 2147483647);
        for v in [0i32, 1, -1, 2, -2, 1000, -1000, i32::MAX, i32::MIN] {
            assert_eq!(zigzag32(zz(v)), v, "value {v}");
        }
    }

    #[test]
    fn unknown_fields_are_skipped() {
        // A tile carrying an unrecognised field 9 of each wire type still
        // decodes. tippecanoe emits fields outside the published schema, so
        // this is not a hypothetical.
        let mut tile = Vec::new();
        varint_field(9, 12345, &mut tile);
        delimited(8, b"junk", &mut tile);
        varint(key(7, WIRE_32BIT), &mut tile);
        tile.extend_from_slice(&[1, 2, 3, 4]);
        varint(key(6, WIRE_64BIT), &mut tile);
        tile.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let layer_tile = tile_with(1, &[command(CMD_MOVE_TO, 1), zz(5), zz(5)], 4096);
        tile.extend_from_slice(&layer_tile);

        let decoded = Tile::decode(&tile).expect("should skip unknown fields");
        assert_eq!(decoded.layers.len(), 1);
    }

    #[test]
    fn reserved_wire_types_are_rejected() {
        // Wire type 3 is a deprecated start-group; nothing valid uses it.
        let mut tile = Vec::new();
        varint(key(9, 3), &mut tile);
        assert_eq!(Tile::decode(&tile), Err(Error::UnknownWireType(3)));
    }

    // ── geometry ─────────────────────────────────────────────────────────

    #[test]
    fn decodes_a_point() {
        let tile = tile_with(1, &[command(CMD_MOVE_TO, 1), zz(25), zz(17)], 4096);
        let t = Tile::decode(&tile).expect("valid tile");
        let layer = t.layer("chart").expect("layer named chart");

        assert_eq!(layer.extent, 4096);
        assert_eq!(layer.version, 2);
        assert_eq!(layer.features[0].geometry, Geometry::Points(vec![[25, 17]]));
    }

    #[test]
    fn decodes_a_multipoint_from_one_move() {
        // One MoveTo with count 2: deltas accumulate, so the second point is
        // at (25+5, 17+5). Treating the delta as absolute is the classic bug.
        let tile = tile_with(
            1,
            &[command(CMD_MOVE_TO, 2), zz(25), zz(17), zz(5), zz(5)],
            4096,
        );
        let t = Tile::decode(&tile).expect("valid tile");
        assert_eq!(
            t.layers[0].features[0].geometry,
            Geometry::Points(vec![[25, 17], [30, 22]])
        );
    }

    #[test]
    fn decodes_a_linestring() {
        let tile = tile_with(
            2,
            &[
                command(CMD_MOVE_TO, 1),
                zz(2),
                zz(2),
                command(CMD_LINE_TO, 2),
                zz(10),
                zz(0),
                zz(0),
                zz(10),
            ],
            4096,
        );
        let t = Tile::decode(&tile).expect("valid tile");
        assert_eq!(
            t.layers[0].features[0].geometry,
            Geometry::Lines(vec![vec![[2, 2], [12, 2], [12, 12]]])
        );
    }

    #[test]
    fn a_second_move_starts_a_second_line() {
        let tile = tile_with(
            2,
            &[
                command(CMD_MOVE_TO, 1),
                zz(0),
                zz(0),
                command(CMD_LINE_TO, 1),
                zz(5),
                zz(0),
                command(CMD_MOVE_TO, 1),
                zz(10),
                zz(10),
                command(CMD_LINE_TO, 1),
                zz(5),
                zz(0),
            ],
            4096,
        );
        let t = Tile::decode(&tile).expect("valid tile");
        let Geometry::Lines(lines) = &t.layers[0].features[0].geometry else {
            panic!("expected lines");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], vec![[0, 0], [5, 0]]);
        // The cursor carries across the MoveTo: (5,0) + (10,10) = (15,10).
        assert_eq!(lines[1], vec![[15, 10], [20, 10]]);
    }

    #[test]
    fn decodes_a_clockwise_polygon_as_an_exterior_ring() {
        // Clockwise in y-south coordinates: (0,0) → (10,0) → (10,10) → (0,10).
        let tile = tile_with(
            3,
            &[
                command(CMD_MOVE_TO, 1),
                zz(0),
                zz(0),
                command(CMD_LINE_TO, 3),
                zz(10),
                zz(0),
                zz(0),
                zz(10),
                zz(-10),
                zz(0),
                command(CMD_CLOSE_PATH, 1),
            ],
            4096,
        );
        let t = Tile::decode(&tile).expect("valid tile");
        let Geometry::Polygons(polys) = &t.layers[0].features[0].geometry else {
            panic!("expected polygons");
        };

        assert_eq!(polys.len(), 1);
        assert!(polys[0].holes.is_empty());
        // ClosePath repeated the first point, so the ring is closed.
        assert_eq!(polys[0].exterior.first(), polys[0].exterior.last());
        assert_eq!(polys[0].exterior.len(), 5);
        assert!(
            signed_area_x2(&polys[0].exterior) > 0,
            "exterior must wind positive"
        );
    }

    #[test]
    fn a_counterclockwise_ring_becomes_a_hole() {
        let mut commands = vec![
            // Exterior, clockwise, 0..20.
            command(CMD_MOVE_TO, 1),
            zz(0),
            zz(0),
            command(CMD_LINE_TO, 3),
            zz(20),
            zz(0),
            zz(0),
            zz(20),
            zz(-20),
            zz(0),
            command(CMD_CLOSE_PATH, 1),
        ];
        // Interior, counterclockwise, 5..15. Cursor is back at (0,0).
        commands.extend_from_slice(&[
            command(CMD_MOVE_TO, 1),
            zz(5),
            zz(5),
            command(CMD_LINE_TO, 3),
            zz(0),
            zz(10),
            zz(10),
            zz(0),
            zz(0),
            zz(-10),
            command(CMD_CLOSE_PATH, 1),
        ]);

        let tile = tile_with(3, &commands, 4096);
        let t = Tile::decode(&tile).expect("valid tile");
        let Geometry::Polygons(polys) = &t.layers[0].features[0].geometry else {
            panic!("expected polygons");
        };

        assert_eq!(polys.len(), 1, "the hole must not become its own polygon");
        assert_eq!(polys[0].holes.len(), 1);
        assert!(
            signed_area_x2(&polys[0].holes[0]) < 0,
            "hole must wind negative"
        );
        assert_eq!(polys[0].rings().count(), 2);
    }

    #[test]
    fn degenerate_rings_are_dropped() {
        // A ring collapsed to a line by simplification: zero area.
        let tile = tile_with(
            3,
            &[
                command(CMD_MOVE_TO, 1),
                zz(0),
                zz(0),
                command(CMD_LINE_TO, 2),
                zz(10),
                zz(0),
                zz(-10),
                zz(0),
                command(CMD_CLOSE_PATH, 1),
            ],
            4096,
        );
        let t = Tile::decode(&tile).expect("valid tile");
        assert_eq!(
            t.layers[0].features[0].geometry,
            Geometry::Polygons(vec![]),
            "a zero-area ring is neither exterior nor interior"
        );
    }

    #[test]
    fn empty_geometry_is_the_matching_empty() {
        for (kind, expected) in [
            (1u32, Geometry::Points(vec![])),
            (2, Geometry::Lines(vec![])),
            (3, Geometry::Polygons(vec![])),
            (0, Geometry::Unknown),
        ] {
            let tile = tile_with(kind, &[], 4096);
            let t = Tile::decode(&tile).expect("valid tile");
            assert_eq!(t.layers[0].features[0].geometry, expected, "kind {kind}");
        }
    }

    #[test]
    fn truncated_command_stream_is_an_error() {
        // Claims three points, supplies one.
        let tile = tile_with(2, &[command(CMD_LINE_TO, 3), zz(1), zz(1)], 4096);
        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    #[test]
    fn line_before_move_is_an_error() {
        let tile = tile_with(2, &[command(CMD_LINE_TO, 1), zz(1), zz(1)], 4096);
        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    #[test]
    fn zero_count_command_is_an_error() {
        let tile = tile_with(2, &[command(CMD_MOVE_TO, 0)], 4096);
        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    #[test]
    fn unknown_command_is_an_error() {
        let tile = tile_with(2, &[command(4, 1), zz(1), zz(1)], 4096);
        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    // ── attributes ───────────────────────────────────────────────────────

    #[test]
    fn resolves_attributes_against_the_layer_tables() {
        let mut feature = Vec::new();
        varint_field(1, 42, &mut feature);
        packed(2, &[0, 0, 1, 1], &mut feature);
        varint_field(3, 1, &mut feature);
        packed(4, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], &mut feature);

        // values[0] = string "shoal", values[1] = double 7.3
        let mut v0 = Vec::new();
        delimited(1, b"shoal", &mut v0);
        let mut v1 = Vec::new();
        varint(key(3, WIRE_64BIT), &mut v1);
        v1.extend_from_slice(&7.3f64.to_bits().to_le_bytes());

        let mut layer = Vec::new();
        delimited(1, b"chart", &mut layer);
        delimited(2, &feature, &mut layer);
        delimited(3, b"name", &mut layer);
        delimited(3, b"depth", &mut layer);
        delimited(4, &v0, &mut layer);
        delimited(4, &v1, &mut layer);
        varint_field(15, 2, &mut layer);

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);

        let t = Tile::decode(&tile).expect("valid tile");
        let f = &t.layers[0].features[0];

        assert_eq!(f.id, Some(42));
        assert_eq!(f.attribute("name"), Some(&Value::String("shoal".into())));
        assert_eq!(f.attribute("depth").and_then(Value::as_f64), Some(7.3));
        assert_eq!(f.attribute("missing"), None);
    }

    #[test]
    fn every_numeric_value_case_reads_as_a_number() {
        let cases: Vec<(Vec<u8>, f64)> = vec![
            {
                let mut v = Vec::new();
                varint(key(2, WIRE_32BIT), &mut v);
                v.extend_from_slice(&2.5f32.to_bits().to_le_bytes());
                (v, 2.5)
            },
            {
                let mut v = Vec::new();
                varint(key(3, WIRE_64BIT), &mut v);
                v.extend_from_slice(&(-3.25f64).to_bits().to_le_bytes());
                (v, -3.25)
            },
            {
                let mut v = Vec::new();
                varint_field(4, 17, &mut v);
                (v, 17.0)
            },
            {
                let mut v = Vec::new();
                varint_field(5, 18, &mut v);
                (v, 18.0)
            },
            {
                let mut v = Vec::new();
                // sint64 field 6 is zigzagged: -9 encodes as 17.
                varint_field(6, 17, &mut v);
                (v, -9.0)
            },
        ];

        for (bytes, expected) in cases {
            let value = decode_value(&bytes).expect("valid value");
            assert_eq!(value.as_f64(), Some(expected), "{value:?}");
        }

        let mut b = Vec::new();
        varint_field(7, 1, &mut b);
        assert_eq!(decode_value(&b), Ok(Value::Bool(true)));
        assert_eq!(
            decode_value(&b).unwrap().as_f64(),
            None,
            "a bool is not a number"
        );
    }

    #[test]
    fn out_of_range_tag_is_an_error() {
        let mut feature = Vec::new();
        // Points at key 5 and value 5 in tables that hold one entry each.
        packed(2, &[5, 5], &mut feature);
        varint_field(3, 1, &mut feature);
        packed(4, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], &mut feature);

        let mut v0 = Vec::new();
        delimited(1, b"x", &mut v0);

        let mut layer = Vec::new();
        delimited(1, b"chart", &mut layer);
        delimited(2, &feature, &mut layer);
        delimited(3, b"k", &mut layer);
        delimited(4, &v0, &mut layer);
        varint_field(15, 2, &mut layer);

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);

        assert_eq!(Tile::decode(&tile), Err(Error::TagOutOfRange));
    }

    #[test]
    fn odd_tag_count_is_an_error() {
        let mut feature = Vec::new();
        packed(2, &[0], &mut feature);
        varint_field(3, 1, &mut feature);
        packed(4, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], &mut feature);

        let mut layer = Vec::new();
        delimited(1, b"chart", &mut layer);
        delimited(2, &feature, &mut layer);
        delimited(3, b"k", &mut layer);
        varint_field(15, 2, &mut layer);

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);

        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    // ── layer sanity ─────────────────────────────────────────────────────

    #[test]
    fn features_decode_even_when_tables_come_last() {
        // The spec does not fix field order, so a writer may put features
        // before the key and value tables. Resolving attributes eagerly
        // while walking fields would fail here.
        let mut feature = Vec::new();
        packed(2, &[0, 0], &mut feature);
        varint_field(3, 1, &mut feature);
        packed(4, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], &mut feature);

        let mut v0 = Vec::new();
        delimited(1, b"late", &mut v0);

        let mut layer = Vec::new();
        delimited(2, &feature, &mut layer);
        delimited(1, b"chart", &mut layer);
        varint_field(15, 2, &mut layer);
        delimited(3, b"k", &mut layer);
        delimited(4, &v0, &mut layer);

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);

        let t = Tile::decode(&tile).expect("field order must not matter");
        assert_eq!(
            t.layers[0].features[0].attribute("k"),
            Some(&Value::String("late".into()))
        );
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut layer = Vec::new();
        delimited(1, b"chart", &mut layer);
        varint_field(15, 99, &mut layer);
        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);
        assert_eq!(Tile::decode(&tile), Err(Error::Version(99)));
    }

    #[test]
    fn zero_extent_is_rejected() {
        // Everything downstream divides by extent.
        let tile = tile_with(1, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], 0);
        assert!(matches!(Tile::decode(&tile), Err(Error::Geometry(_))));
    }

    #[test]
    fn a_non_default_extent_is_honoured() {
        let tile = tile_with(1, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], 512);
        let t = Tile::decode(&tile).expect("valid tile");
        assert_eq!(t.layers[0].extent, 512);
    }

    #[test]
    fn an_empty_tile_decodes_to_no_layers() {
        assert_eq!(Tile::decode(&[]), Ok(Tile { layers: vec![] }));
    }

    #[test]
    fn missing_layer_lookup_is_none() {
        let tile = tile_with(1, &[command(CMD_MOVE_TO, 1), zz(1), zz(1)], 4096);
        let t = Tile::decode(&tile).expect("valid tile");
        assert!(t.layer("DEPARE").is_none());
    }
}
