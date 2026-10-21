//! Bounded subset of progressive, 8-bit 4:2:0 AVC used by Gliff's I/P stream.
use crate::{Error, Result};
pub const MAX_AU: usize = 64 * 1024 * 1024;
const MAX_NALS: usize = 4096;
const MAX_PARAMETER: usize = 64 * 1024;

fn invalid(message: &str) -> Error {
    Error::Unsupported(message.into())
}

/// No allocation: at most 4096 borrowed NALs, each inside the bounded AU.
fn nals(data: &[u8]) -> Result<Vec<&[u8]>> {
    if data.is_empty() || data.len() > MAX_AU {
        return Err(invalid("access unit size limit"));
    }
    let mut result = Vec::new();
    let mut start = None;
    let mut zeros = 0;
    for (i, &byte) in data.iter().enumerate() {
        if byte == 1 && zeros >= 2 {
            let prefix = i - zeros;
            if let Some(begin) = start {
                if prefix == begin {
                    return Err(invalid("empty NAL"));
                }
                result.push(&data[begin..prefix]);
                if result.len() >= MAX_NALS {
                    return Err(invalid("too many NALs"));
                }
            } else if data[..prefix].iter().any(|&b| b != 0) {
                return Err(invalid("data before Annex-B prefix"));
            }
            start = Some(i + 1);
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let start = start.ok_or_else(|| invalid("missing Annex-B prefix"))?;
    let end = data.len() - zeros;
    if start >= end {
        return Err(invalid("empty final NAL"));
    }
    result.push(&data[start..end]);
    Ok(result)
}

/// Unescape only the small header prefix we parse; compressed slices stay borrowed.
fn rbsp(nal: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut zeros = 0;
    for (i, &byte) in nal.iter().skip(1).take(MAX_PARAMETER).enumerate() {
        if zeros >= 2 && byte == 3 {
            if nal.get(i + 2).is_none_or(|&next| next > 3) {
                return Err(invalid("bad emulation prevention"));
            }
            zeros = 0;
            continue;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    Ok(result)
}
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn get(&mut self, n: usize) -> Result<u32> {
        if n > 32
            || self
                .pos
                .checked_add(n)
                .is_none_or(|p| p > self.data.len() * 8)
        {
            return Err(invalid("truncated AVC header"));
        }
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | ((self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32> {
        let mut n = 0;
        while self.get(1)? == 0 {
            n += 1;
            if n > 24 {
                return Err(invalid("AVC integer limit"));
            }
        }
        Ok(((1u32 << n) - 1) + self.get(n)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameters {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}
#[derive(Debug)]
struct Sps {
    id: u32,
    width: u32,
    height: u32,
}
fn sps(nal: &[u8]) -> Result<Sps> {
    let raw = rbsp(nal)?;
    let mut bits = Bits::new(&raw);
    let profile = bits.get(8)?;
    bits.get(16)?;
    let id = bits.ue()?;
    if id > 31 {
        return Err(invalid("SPS id limit"));
    }
    match profile {
        66 | 77 | 88 => {}
        100 => {
            if bits.ue()? != 1 || bits.ue()? != 0 || bits.ue()? != 0 {
                return Err(invalid("only 8-bit 4:2:0 supported"));
            }
            bits.get(1)?;
            if bits.get(1)? != 0 {
                return Err(invalid("custom scaling matrices unsupported"));
            }
        }
        _ => return Err(invalid("unsupported AVC profile")),
    }
    if bits.ue()? > 12 {
        return Err(invalid("frame number limit"));
    }
    match bits.ue()? {
        0 => {
            if bits.ue()? > 12 {
                return Err(invalid("POC limit"));
            }
        }
        1 => {
            bits.get(1)?;
            bits.ue()?;
            bits.ue()?;
            let count = bits.ue()?;
            if count > 256 {
                return Err(invalid("POC cycle limit"));
            }
            for _ in 0..count {
                bits.ue()?;
            }
        }
        2 => {}
        _ => return Err(invalid("invalid POC type")),
    }
    if bits.ue()? > 16 {
        return Err(invalid("reference frame limit"));
    }
    bits.get(1)?;
    let width = (bits.ue()? + 1)
        .checked_mul(16)
        .ok_or_else(|| invalid("coded width overflow"))?;
    let height = (bits.ue()? + 1)
        .checked_mul(16)
        .ok_or_else(|| invalid("coded height overflow"))?;
    if bits.get(1)? != 1 || width > 4096 || height > 4096 {
        return Err(invalid("coded dimensions or interlace unsupported"));
    }
    bits.get(1)?;
    let (mut crop_x, mut crop_y) = (0, 0);
    if bits.get(1)? != 0 {
        crop_x = (bits.ue()? + bits.ue()?)
            .checked_mul(2)
            .ok_or_else(|| invalid("crop overflow"))?;
        crop_y = (bits.ue()? + bits.ue()?)
            .checked_mul(2)
            .ok_or_else(|| invalid("crop overflow"))?;
    }
    if crop_x >= width || crop_y >= height {
        return Err(invalid("invalid SPS crop"));
    }
    Ok(Sps {
        id,
        width: width - crop_x,
        height: height - crop_y,
    })
}
#[derive(Default)]
pub struct Stream {
    parameters: Option<Parameters>,
}
pub struct Unit {
    pub avcc: Vec<u8>,
    pub parameters: Parameters,
    pub changed: bool,
}
impl Stream {
    pub fn parse(&mut self, data: &[u8], expected: (u32, u32)) -> Result<Unit> {
        let units = nals(data)?;
        let (mut new_sps, mut new_pps) = (None, None);
        let mut saw_vcl = false;
        for &nal in &units {
            if nal[0] & 0x80 != 0 {
                return Err(invalid("forbidden NAL bit"));
            }
            match nal[0] & 31 {
                7 | 8 => {
                    if saw_vcl || nal.len() > MAX_PARAMETER {
                        return Err(invalid("parameter size/order limit"));
                    }
                    let slot = if nal[0] & 31 == 7 {
                        &mut new_sps
                    } else {
                        &mut new_pps
                    };
                    if slot.is_some_and(|old: &[u8]| old != nal) {
                        return Err(invalid("multiple parameter sets unsupported"));
                    }
                    *slot = Some(nal);
                }
                1 | 5 => saw_vcl = true,
                6 | 9 | 10 | 11 | 12 => {}
                _ => return Err(invalid("unsupported NAL type")),
            }
        }
        let previous = self.parameters.as_ref();
        let parameters = Parameters {
            sps: new_sps
                .or_else(|| previous.map(|p| p.sps.as_slice()))
                .ok_or_else(|| invalid("missing SPS"))?
                .to_vec(),
            pps: new_pps
                .or_else(|| previous.map(|p| p.pps.as_slice()))
                .ok_or_else(|| invalid("missing PPS"))?
                .to_vec(),
        };
        let geometry = sps(&parameters.sps)?;
        if (geometry.width, geometry.height) != expected {
            return Err(invalid("SPS geometry differs from StreamConfig"));
        }
        let raw = rbsp(&parameters.pps)?;
        let mut pps = Bits::new(&raw);
        let pps_id = pps.ue()?;
        if pps_id > 255 || pps.ue()? != geometry.id {
            return Err(invalid("PPS references unknown SPS"));
        }
        // Reject FMO, for which first_mb ordering and one-AU checks are insufficient.
        pps.get(2)?;
        if pps.ue()? != 0 {
            return Err(invalid("slice groups unsupported"));
        }
        let changed = previous != Some(&parameters);
        let mut first_mb = None;
        let mut kind = None;
        let mut avcc = Vec::with_capacity(data.len().min(MAX_AU));
        for nal in units {
            let nal_type = nal[0] & 31;
            if matches!(nal_type, 1 | 5) {
                let raw = rbsp(nal)?;
                let mut slice = Bits::new(&raw);
                let mb = slice.ue()?;
                let slice_type = slice.ue()?;
                if !matches!(slice_type, 0 | 2 | 5 | 7)
                    || (nal_type == 5 && slice_type % 5 != 2)
                    || slice.ue()? != pps_id
                {
                    return Err(invalid("unsupported slice or PPS"));
                }
                if first_mb.is_none_or(|old| mb <= old) && (first_mb.is_some() || mb != 0) {
                    return Err(invalid("multiple pictures or invalid slice order"));
                }
                if kind.is_some_and(|old| old != nal_type) {
                    return Err(invalid("mixed IDR and non-IDR"));
                }
                first_mb = Some(mb);
                kind = Some(nal_type);
            }
            if !matches!(nal_type, 7..=12) {
                let size = avcc
                    .len()
                    .checked_add(nal.len() + 4)
                    .ok_or_else(|| invalid("AVCC overflow"))?;
                if size > MAX_AU {
                    return Err(invalid("AVCC size limit"));
                }
                avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                avcc.extend_from_slice(nal);
            }
        }
        if first_mb.is_none() || (changed && kind != Some(5)) {
            return Err(invalid("new format needs an IDR picture"));
        }
        self.parameters = Some(parameters.clone());
        Ok(Unit {
            avcc,
            parameters,
            changed,
        })
    }
    pub fn reset(&mut self) {
        self.parameters = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const IDR: &[u8] = include_bytes!("../tests/fixtures/64x48-0.h264");
    const P: &[u8] = include_bytes!("../tests/fixtures/64x48-1.h264");
    #[test]
    fn mixed_prefixes_empty_truncated_and_limits() {
        assert_eq!(
            nals(&[0, 0, 1, 0x65, 2, 0, 0, 0, 1, 0x61, 3]).unwrap(),
            vec![&[0x65, 2][..], &[0x61, 3][..]]
        );
        for data in [
            &[][..],
            &[1, 2],
            &[1, 0, 0, 1, 0x65],
            &[0, 0, 1],
            &[0, 0, 1, 0, 0, 1, 0x65],
        ] {
            assert!(nals(data).is_err());
        }
        let huge = vec![0; MAX_AU + 1];
        assert!(nals(&huge).is_err());
        assert!(nals(&[0, 0, 1, 0x61].repeat(MAX_NALS + 1)).is_err());
        assert!(rbsp(&[0x67, 0, 0, 3, 4]).is_err());
        assert!(Bits::new(&[0; 16]).ue().is_err());
    }
    #[test]
    fn caches_sets_repeats_and_rejects_geometry_without_poisoning() {
        let mut stream = Stream::default();
        assert!(stream.parse(P, (64, 48)).is_err());
        assert!(stream.parse(IDR, (80, 64)).is_err());
        assert!(stream.parse(IDR, (64, 48)).unwrap().changed);
        assert!(!stream.parse(P, (64, 48)).unwrap().changed);
        assert!(!stream.parse(IDR, (64, 48)).unwrap().changed);
        let bad = [0, 0, 1, 0xe5];
        assert!(stream.parse(&bad, (64, 48)).is_err());
        assert!(!stream.parse(P, (64, 48)).unwrap().changed);
    }
    #[test]
    fn avcc_multiple_slices_and_parameter_limits() {
        let source = include_bytes!("../tests/fixtures/128x96-0.h264");
        let unit = Stream::default().parse(source, (128, 96)).unwrap();
        let mut avcc = unit.avcc.as_slice();
        let mut count = 0;
        while !avcc.is_empty() {
            let len = u32::from_be_bytes(avcc[..4].try_into().unwrap()) as usize;
            assert_eq!(avcc[4] & 31, 5);
            avcc = &avcc[4 + len..];
            count += 1;
        }
        assert_eq!(count, 6);
        let mut oversized = vec![0, 0, 1, 0x67];
        oversized.extend(vec![0xff; MAX_PARAMETER]);
        oversized.extend_from_slice(IDR);
        assert!(Stream::default().parse(&oversized, (64, 48)).is_err());
        let mut forbidden = IDR.to_vec();
        forbidden[5] = 244; // unsupported SPS profile
        assert!(Stream::default().parse(&forbidden, (64, 48)).is_err());
        let slices = [P, P].concat();
        let mut stream = Stream::default();
        stream.parse(IDR, (64, 48)).unwrap();
        assert!(stream.parse(&slices, (64, 48)).is_err());
    }

    #[test]
    fn cropped_non_macroblock_dimensions_are_preserved() {
        let mut stream = Stream::default();
        assert!(stream
            .parse(include_bytes!("../tests/fixtures/34x18-0.h264"), (34, 18))
            .is_ok());
        assert!(
            stream
                .parse(include_bytes!("../tests/fixtures/16x16-0.h264"), (16, 16))
                .unwrap()
                .changed
        );
    }
    #[test]
    fn deterministic_random_malformed_inputs_are_bounded() {
        let mut random = 1u32;
        for len in 0..512 {
            let data: Vec<_> = (0..len)
                .map(|_| {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    random as u8
                })
                .collect();
            let _ = Stream::default().parse(&data, (64, 48));
        }
    }
}
