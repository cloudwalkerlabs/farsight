//! H.264 and HEVC in Annex B, as the server sends them.

use farsight_proto::codec::Codec as CodecId;

/// Splits an Annex B keyframe into its parameter sets (SPS and PPS, and
/// VPS for HEVC), which MediaCodec wants as codec config, and the rest.
pub fn split_parameter_sets(codec: CodecId, data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut config, mut picture) = (Vec::new(), Vec::new());
    for nal in nal_units(data) {
        let Some(&first) = nal.get(start_code_len(nal)) else { continue };
        let is_config = match codec {
            CodecId::H264 => matches!(first & 0x1f, 7 | 8),
            CodecId::Hevc => matches!((first >> 1) & 0x3f, 32..=34),
            CodecId::Av1 => false,
        };
        if is_config { &mut config } else { &mut picture }.extend_from_slice(nal);
    }
    (config, picture)
}

fn start_code_len(nal: &[u8]) -> usize {
    if nal.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 }
}

/// The NAL units of an Annex B stream, each with its start code.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // A four-byte start code begins one earlier.
            starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    starts.iter().enumerate().map(|(k, &s)| &data[s..starts.get(k + 1).copied().unwrap_or(data.len())]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_sets_are_split_off() {
        let sps = [0, 0, 0, 1, 0x67, 1, 2];
        let pps = [0, 0, 0, 1, 0x68, 3];
        let idr = [0, 0, 1, 0x65, 4, 5, 0, 0, 1, 0x65, 6];
        let data = [&sps[..], &pps, &idr].concat();
        let (config, picture) = split_parameter_sets(CodecId::H264, &data);
        assert_eq!(config, [&sps[..], &pps].concat());
        assert_eq!(picture, idr);
        let vps = [0, 0, 0, 1, 0x40, 1];
        let slice = [0, 0, 0, 1, 0x26, 1];
        let (config, picture) = split_parameter_sets(CodecId::Hevc, &[&vps[..], &slice].concat());
        assert_eq!((config, picture), (vps.to_vec(), slice.to_vec()));
    }
}
