//! G.711 µ-law ↔ signed 16-bit linear PCM conversion.
//!
//! Modem training requires the cleanest possible audio path. Converting
//! slin↔ulaw in the bridge gives direct codec control — no transcoding
//! middleman, no resampling, no re-timing. The bridge sends PCMU directly
//! over RTP to the SIP trunk.
//!
//! Algorithm: ITU-T G.711 (1988), public domain reference implementation
//! by Craig Reese (IDA/Supercomputing Research Center) and Joe Campbell
//! (Department of Defense), 29 September 1989.

/// Bias added before µ-law compression (avoids log(0) in the encoding).
const BIAS: i32 = 0x84;

/// Maximum linear magnitude before clipping. Values above this are clamped
/// to prevent overflow in the segment lookup.
const CLIP: i32 = 32635;

/// Segment lookup table for µ-law encoding. Maps (biased_sample >> 7) to
/// the segment number (0–7). Each segment doubles the step size, giving
/// µ-law its logarithmic compression characteristic.
#[rustfmt::skip]
const EXP_LUT: [u8; 256] = [
    0,0,1,1,2,2,2,2,3,3,3,3,3,3,3,3,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,
    6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
    6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
    6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
    6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
];

/// Encode a single 16-bit signed linear sample to 8-bit µ-law.
///
/// Steps: extract sign → clip → add bias → find segment via lookup →
/// extract 4-bit mantissa → combine and invert all bits.
pub fn slin_sample_to_ulaw(sample: i16) -> u8 {
    let sign = ((sample >> 8) as u8) & 0x80;
    let mut magnitude = if sign != 0 {
        -(sample as i32)
    } else {
        sample as i32
    };

    if magnitude > CLIP {
        magnitude = CLIP;
    }
    magnitude += BIAS;

    let exponent = EXP_LUT[((magnitude >> 7) & 0xFF) as usize];
    let mantissa = ((magnitude >> (exponent as i32 + 3)) & 0x0F) as u8;

    !(sign | (exponent << 4) | mantissa)
}

/// Decode a single 8-bit µ-law byte to 16-bit signed linear.
///
/// Steps: invert all bits → extract sign, exponent, mantissa →
/// reconstruct linear value → remove bias → apply sign.
pub fn ulaw_sample_to_slin(u_val: u8) -> i16 {
    let u_val = !u_val;
    let sign = u_val & 0x80;
    let exponent = ((u_val & 0x70) >> 4) as i32;
    let mantissa = (u_val & 0x0F) as i32;

    // Reconstruct: place mantissa in position, add half-step bias, shift by
    // exponent, then remove the encoding bias.
    let mut sample = ((mantissa << 3) + BIAS) << exponent;
    sample -= BIAS;

    // In µ-law, the sign bit in the complemented byte indicates negative.
    // sample already holds (t - BIAS), which is the positive magnitude.
    if sign != 0 {
        -(sample as i16)
    } else {
        sample as i16
    }
}

/// Convert a buffer of signed 16-bit little-endian PCM to µ-law.
///
/// Input must contain an even number of bytes (whole 16-bit samples).
/// Output is half the input size (one byte per sample).
pub fn encode_ulaw(slin: &[u8]) -> Vec<u8> {
    assert!(
        slin.len() % 2 == 0,
        "slin buffer must contain whole 16-bit samples, got {} bytes",
        slin.len()
    );

    let mut ulaw = Vec::with_capacity(slin.len() / 2);
    for chunk in slin.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        ulaw.push(slin_sample_to_ulaw(sample));
    }
    ulaw
}

/// Convert a buffer of µ-law to signed 16-bit little-endian PCM.
///
/// Output is twice the input size (two bytes per sample).
pub fn decode_ulaw(ulaw: &[u8]) -> Vec<u8> {
    let mut slin = Vec::with_capacity(ulaw.len() * 2);
    for &byte in ulaw {
        let sample = ulaw_sample_to_slin(byte);
        slin.extend_from_slice(&sample.to_le_bytes());
    }
    slin
}

/// Combine any residual byte from a previous read with new data to form
/// a complete slin buffer (even byte count for 16-bit samples).
///
/// Why: Unix socket reads may split mid-sample (extremely rare but possible
/// after signal interrupts). We buffer the trailing odd byte and prepend it
/// to the next read to maintain sample alignment.
pub fn align_slin_buffer(residual: &mut Option<u8>, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(data.len() + 1);
    if let Some(r) = residual.take() {
        buf.push(r);
    }
    buf.extend_from_slice(data);
    if buf.len() % 2 != 0 {
        *residual = Some(buf.pop().expect("buf is non-empty when odd"));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_roundtrips() {
        // Digital silence: slin 0 encodes to µ-law 0xFF, decodes back to 0.
        assert_eq!(slin_sample_to_ulaw(0), 0xFF);
        assert_eq!(ulaw_sample_to_slin(0xFF), 0);
    }

    #[test]
    fn max_positive_roundtrip_within_quantization() {
        let ulaw = slin_sample_to_ulaw(i16::MAX);
        let back = ulaw_sample_to_slin(ulaw);
        // µ-law quantization at high amplitudes loses precision;
        // error must be < 1% of full scale (327 out of 32767).
        let error = (back as i32 - i16::MAX as i32).unsigned_abs();
        assert!(error < 700, "roundtrip error {error} exceeds tolerance");
    }

    #[test]
    fn max_negative_roundtrip_within_quantization() {
        // Use MIN+1 to avoid i16::MIN negation overflow.
        let ulaw = slin_sample_to_ulaw(i16::MIN + 1);
        let back = ulaw_sample_to_slin(ulaw);
        let error = (back as i32 - (i16::MIN as i32 + 1)).unsigned_abs();
        assert!(error < 700, "roundtrip error {error} exceeds tolerance");
    }

    #[test]
    fn small_values_have_fine_quantization() {
        // µ-law has its finest resolution near zero (where modem training
        // tones live). Roundtrip error should be very small.
        for sample in [-100i16, -10, -1, 1, 10, 100] {
            let ulaw = slin_sample_to_ulaw(sample);
            let back = ulaw_sample_to_slin(ulaw);
            let error = (back as i32 - sample as i32).unsigned_abs();
            assert!(
                error < 8,
                "sample {sample}: roundtrip error {error} too large for small value"
            );
        }
    }

    #[test]
    fn buffer_encode_decode_preserves_length() {
        let slin_input: Vec<u8> = vec![0, 0, 0xFF, 0x7F, 0x01, 0x80];
        let ulaw = encode_ulaw(&slin_input);
        assert_eq!(ulaw.len(), 3, "ulaw should be half the slin size");
        let slin_output = decode_ulaw(&ulaw);
        assert_eq!(slin_output.len(), 6, "decoded slin should be twice ulaw size");
    }

    #[test]
    fn encode_rejects_odd_byte_count() {
        let result = std::panic::catch_unwind(|| encode_ulaw(&[1, 2, 3]));
        assert!(result.is_err(), "odd byte count must panic via assertion");
    }

    #[test]
    fn align_handles_even_input() {
        let mut residual = None;
        let result = align_slin_buffer(&mut residual, &[1, 2, 3, 4]);
        assert_eq!(result, vec![1, 2, 3, 4]);
        assert!(residual.is_none());
    }

    #[test]
    fn align_handles_odd_input() {
        let mut residual = None;
        let result = align_slin_buffer(&mut residual, &[1, 2, 3]);
        assert_eq!(result, vec![1, 2]);
        assert_eq!(residual, Some(3));
    }

    #[test]
    fn align_prepends_residual() {
        let mut residual = Some(0xAA);
        let result = align_slin_buffer(&mut residual, &[0xBB, 0xCC, 0xDD]);
        assert_eq!(result, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert!(residual.is_none());
    }
}
