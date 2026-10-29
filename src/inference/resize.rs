//! Pixel-exact replica of Pillow's BICUBIC resize as used by MinerU 4.0.12
//! (`pp_doclayout_v2_onnx.py::_preprocess_single_image`): a = -0.75 kernel,
//! half-pixel coordinate centers, edge clamping, uint8 fixed-point weights
//! (rint(w * 2048)) in the horizontal pass, and a split float/integer vertical
//! accumulation (f32 mul_add path below the 8-lane boundary, i64 path above).

pub(crate) fn resize_bicubic(
    src: &[u8],
    sw: usize,
    sh: usize,
    tw: usize,
    th: usize,
    channels: usize,
) -> Vec<u8> {
    assert_eq!(src.len(), sw * sh * channels, "resize input size mismatch");
    if (sw, sh) == (tw, th) {
        return src.to_vec();
    }
    let xs = taps(sw, tw);
    let ys = taps(sh, th);
    let mut out = vec![0u8; tw * th * channels];
    // Cache only the source rows the current output row needs, like upstream's
    // `rows.retain(...)`; peak memory stays at four horizontal rows.
    let mut cache: Vec<(usize, Vec<f32>)> = Vec::with_capacity(4);
    for (ty, &(base_y, wy)) in ys.iter().enumerate() {
        let needed: [usize; 4] =
            std::array::from_fn(|tap| (base_y + tap as i64 - 1).clamp(0, sh as i64 - 1) as usize);
        cache.retain(|(row, _)| needed.contains(row));
        for &sy in &needed {
            if !cache.iter().any(|(row, _)| *row == sy) {
                cache.push((sy, horizontal_pass(src, sy, sw, tw, channels, &xs)));
            }
        }
        let rows: [&[f32]; 4] = std::array::from_fn(|tap| {
            cache
                .iter()
                .find(|(row, _)| *row == needed[tap])
                .expect("needed row is cached")
                .1
                .as_slice()
        });
        vertical_pass(
            &rows,
            &wy,
            &mut out[ty * tw * channels..(ty + 1) * tw * channels],
        );
    }
    out
}

/// (base index, quantized weights) for one axis, matching upstream's
/// `((i + 0.5) * scale - 0.5)` coordinate mapping with scale = 1 / (t / n).
fn taps(n: usize, t: usize) -> Vec<(i64, [f32; 4])> {
    let scale = 1.0 / (t as f64 / n as f64);
    (0..t)
        .map(|i| {
            let coord = ((i as f64 + 0.5) * scale - 0.5) as f32;
            let base = coord.floor() as i64;
            let frac = coord - base as f32;
            (base, cubic_weights(frac))
        })
        .collect()
}

fn cubic_weights(frac: f32) -> [f32; 4] {
    let a = -0.75f32;
    let p = frac + 1.0;
    let w0 = (a)
        .mul_add(p, -5.0 * a)
        .mul_add(p, 8.0 * a)
        .mul_add(p, -4.0 * a);
    let t1 = (a + 2.0).mul_add(frac, -(a + 3.0)) * frac;
    let w1 = t1.mul_add(frac, 1.0);
    let q = 1.0 - frac;
    let t2 = (a + 2.0).mul_add(q, -(a + 3.0)) * q;
    let w2 = t2.mul_add(q, 1.0);
    let w3 = 1.0 - w0 - w1 - w2;
    // Quantize to 1/2048 fixed point, as upstream feeds np.rint(w * 2048) back
    // through f32 arithmetic.
    [w0, w1, w2, w3].map(|w| (w * 2048.0).round_ties_even())
}

fn horizontal_pass(
    src: &[u8],
    y: usize,
    sw: usize,
    tw: usize,
    channels: usize,
    xs: &[(i64, [f32; 4])],
) -> Vec<f32> {
    let row = &src[y * sw * channels..(y + 1) * sw * channels];
    let mut out = vec![0f32; tw * channels];
    for (tx, &(base_x, wx)) in xs.iter().enumerate() {
        for c in 0..channels {
            let mut sum = 0f32;
            for (tap, &weight) in wx.iter().enumerate() {
                let sx = (base_x + tap as i64 - 1).clamp(0, sw as i64 - 1) as usize;
                sum += f32::from(row[sx * channels + c]) * weight;
            }
            out[tx * channels + c] = sum;
        }
    }
    out
}

fn vertical_pass(rows: &[&[f32]; 4], wy: &[f32; 4], out: &mut [u8]) {
    let flat = out.len();
    let vectorized = flat / 8 * 8;
    for (x, slot) in out.iter_mut().enumerate() {
        if x < vectorized {
            let mut sum = rows[3][x] * (wy[3] / 4_194_304.0);
            for tap in (0..3).rev() {
                sum = rows[tap][x].mul_add(wy[tap] / 4_194_304.0, sum);
            }
            *slot = sum.round_ties_even().clamp(0.0, 255.0) as u8;
        } else {
            let sum: i64 = (0..4).map(|tap| rows[tap][x] as i64 * wy[tap] as i64).sum();
            *slot = ((sum + (1 << 21)) >> 22).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
#[path = "resize_tests.rs"]
mod tests;
