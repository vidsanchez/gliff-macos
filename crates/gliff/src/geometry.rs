//! Pure geometry shared with the remote output's fractional-scale rules.
//! Extracted from upstream hypr-ipc; no IPC or compositor dependency.

/// Round a mode down until `scale` divides it into whole logical pixels.
/// Hyprland rejects a scale that leaves a fractional logical size ("Invalid
/// scale ... failed to find a clean divisor") and falls back to one of its
/// own, so a 1366x768 window at 1.25 would come up at scale 1. A scale of p/q
/// in lowest terms needs a mode that is a multiple of p; the result stays
/// even for the encoder. A scale with no small denominator is left alone.
pub fn clean_mode(width: u32, height: u32, scale: f32) -> (u32, u32) {
    let s = f64::from(scale);
    let Some(q) = (1..=8u32).find(|q| {
        let numerator = s * f64::from(*q);
        (numerator - numerator.round()).abs() < 1e-4
    }) else {
        return (width, height);
    };
    let p = (s * f64::from(q)).round() as u32;
    let step = if p.is_multiple_of(2) { p } else { 2 * p };
    let fit = |v: u32| (v - v % step).max(step);
    (fit(width), fit(height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clean_mode_divides_into_whole_logical_pixels() {
        // Already clean: unchanged.
        assert_eq!(clean_mode(1290, 1926, 3.0), (1290, 1926));
        assert_eq!(clean_mode(2560, 1440, 2.0), (2560, 1440));
        assert_eq!(clean_mode(1920, 1080, 1.0), (1920, 1080));
        // 1.25 = 5/4: multiples of 5, kept even.
        assert_eq!(clean_mode(1366, 768, 1.25), (1360, 760));
        // 3x with an odd leftover: multiples of 6.
        assert_eq!(clean_mode(1290, 1925, 3.0), (1290, 1920));
        // 1.5 = 3/2 and 2.5 = 5/2.
        assert_eq!(clean_mode(1001, 701, 1.5), (996, 696));
        assert_eq!(clean_mode(1083, 2341, 2.5), (1080, 2340));
        // f32 rounds these scales slightly below their exact fractions.
        assert_eq!(clean_mode(1366, 768, 1.4), (1358, 756));
        assert_eq!(clean_mode(1366, 768, 1.8), (1350, 756));
        for (w, h, s) in [(1366, 768, 1.25), (1290, 1925, 3.0), (1001, 701, 1.5)] {
            let (cw, ch) = clean_mode(w, h, s);
            assert_eq!((f64::from(cw) / f64::from(s)).fract(), 0.0);
            assert_eq!((f64::from(ch) / f64::from(s)).fract(), 0.0);
            assert!(cw.is_multiple_of(2) && ch.is_multiple_of(2));
        }
    }
}
