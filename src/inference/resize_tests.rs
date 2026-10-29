use super::resize_bicubic;

#[derive(serde::Deserialize)]
struct GoldenCase {
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    src: String,
    dst: String,
}

fn fixture_cases() -> Vec<GoldenCase> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/layout/resize-golden.json"
    );
    let raw = std::fs::read_to_string(path).expect("golden resize fixture is readable");
    serde_json::from_str(&raw).expect("golden resize fixture is valid JSON")
}

fn decode_hex(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex fixture has odd length");
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex fixture is valid"))
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Byte-exact differential check against the real docvortex 0.6.0
/// `resize_image` (Pillow BICUBIC replica used by MinerU 4.0.12).
#[test]
fn resize_matches_official_bicubic_golden_cases() {
    for case in fixture_cases() {
        let src = decode_hex(&case.src);
        let expected = decode_hex(&case.dst);
        assert_eq!(src.len(), case.w * case.h * 3, "case {}x{}", case.w, case.h);
        assert_eq!(
            expected.len(),
            case.tw * case.th * 3,
            "case {}x{}",
            case.w,
            case.h
        );
        let actual = resize_bicubic(&src, case.w, case.h, case.tw, case.th, 3);
        assert_eq!(
            encode_hex(&actual),
            case.dst,
            "resize mismatch for {}x{} -> {}x{}",
            case.w,
            case.h,
            case.tw,
            case.th
        );
    }
}

/// Full-size differential case (800x600 -> 800x800) generated from the real
/// docvortex 0.6.0 implementation; compared by hash to keep the fixture small.
#[test]
fn resize_matches_official_output_at_layout_input_size() {
    let (w, h) = (800usize, 600usize);
    let mut src = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            for c in 0..3usize {
                src.push(((x * 37 + y * 53 + c * 97) % 256) as u8);
            }
        }
    }
    let out = resize_bicubic(&src, w, h, 800, 800, 3);
    assert_eq!(out.len(), 800 * 800 * 3);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in out {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    assert_eq!(hash, 0x5d01_cf84_a295_3026);
}

#[test]
fn resize_short_circuits_when_sizes_match() {
    let src: Vec<u8> = (0..24u8).collect();
    assert_eq!(resize_bicubic(&src, 4, 2, 4, 2, 3), src);
}

#[test]
fn cubic_weights_symmetry_and_partition_of_unity() {
    for frac in [0.0f32, 0.25, 0.5, 0.730_473_5, 0.999_489_8] {
        let [w0, w1, w2, w3] = super::cubic_weights(frac);
        // Returned weights are already quantized to 1/2048 fixed point.
        let sum = w0 + w1 + w2 + w3;
        assert!((sum - 2048.0).abs() <= 1.0, "weights must sum to 2048");
        // Even symmetry of the kernel around frac = 0.5 (within quantization).
        let [v0, v1, _, _] = super::cubic_weights(1.0 - frac);
        assert!((v0 - w3).abs() <= 1.0);
        assert!((v1 - w2).abs() <= 1.0);
    }
}
