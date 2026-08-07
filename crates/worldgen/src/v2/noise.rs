pub fn mix(seed: u64, x: i32, y: i32) -> u64 {
    let mut value = seed
        ^ u64::from(u32::from_ne_bytes(x.to_ne_bytes())).wrapping_mul(0x9E37_79B1_85EB_CA87)
        ^ u64::from(u32::from_ne_bytes(y.to_ne_bytes())).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

pub fn pass_seed(root: u64, name: &str) -> u64 {
    name.bytes()
        .fold(root ^ 0xA076_1D64_78BD_642F, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0xE703_7ED1_A0B4_28DB)
        })
}

fn smooth(value: i64) -> i64 {
    // Q16 smoothstep: t²(3 - 2t).
    let square = (value * value) >> 16;
    (square * (3 * 65_536 - 2 * value)) >> 16
}

fn lerp(first: i64, second: i64, amount: i64) -> i64 {
    first + (((second - first) * amount) >> 16)
}

/// Interpolated deterministic value noise in approximately `-1024..=1024`.
pub fn value_noise(seed: u64, x: u32, y: u32, scale: u32) -> i32 {
    let scale = scale.max(1);
    let grid_x = x / scale;
    let grid_y = y / scale;
    let local_x = i64::from(x % scale) * 65_536 / i64::from(scale);
    let local_y = i64::from(y % scale) * 65_536 / i64::from(scale);
    let sample = |dx: u32, dy: u32| {
        let hash = mix(
            seed,
            i32::try_from(grid_x + dx).unwrap_or(i32::MAX),
            i32::try_from(grid_y + dy).unwrap_or(i32::MAX),
        );
        i64::try_from(hash % 2_049).unwrap_or_default() - 1_024
    };
    let x_amount = smooth(local_x);
    let y_amount = smooth(local_y);
    let top = lerp(sample(0, 0), sample(1, 0), x_amount);
    let bottom = lerp(sample(0, 1), sample(1, 1), x_amount);
    i32::try_from(lerp(top, bottom, y_amount)).unwrap_or_default()
}

pub fn fractal_noise(seed: u64, x: u32, y: u32, base_scale: u32) -> i32 {
    let scales = [
        base_scale.max(1),
        (base_scale / 2).max(1),
        (base_scale / 4).max(1),
    ];
    let weights = [5_i32, 3, 1];
    scales
        .into_iter()
        .zip(weights)
        .enumerate()
        .map(|(octave, (scale, weight))| {
            value_noise(
                seed ^ (octave as u64).wrapping_mul(0x9E37_79B9),
                x,
                y,
                scale,
            ) * weight
        })
        .sum::<i32>()
        / weights.into_iter().sum::<i32>()
}
