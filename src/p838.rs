//! Specific attenuation from rain, ITU-R P.838-3: gamma = k R^alpha dB per km in rain of
//! R mm/h, with k and alpha from the recommendation's fits in frequency (its Tables 1 to
//! 4), for horizontal and vertical polarisation.

struct Fit {
    a: &'static [f64],
    b: &'static [f64],
    c: &'static [f64],
    m: f64,
    k: f64,
}

impl Fit {
    fn at(&self, ghz: f64) -> f64 {
        let x = ghz.log10();
        let bumps: f64 = self
            .a
            .iter()
            .zip(self.b)
            .zip(self.c)
            .map(|((a, b), c)| a * (-((x - b) / c).powi(2)).exp())
            .sum();
        bumps + self.m * x + self.k
    }
}

/// log10 of k, horizontal
const KH: Fit = Fit {
    a: &[-5.33980, -0.35351, -0.23789, -0.94158],
    b: &[-0.10008, 1.26970, 0.86036, 0.64552],
    c: &[1.13098, 0.45400, 0.15354, 0.16817],
    m: -0.18961,
    k: 0.71147,
};

/// log10 of k, vertical
const KV: Fit = Fit {
    a: &[-3.80595, -3.44965, -0.39902, 0.50167],
    b: &[0.56934, -0.22911, 0.73042, 1.07319],
    c: &[0.81061, 0.51059, 0.11899, 0.27195],
    m: -0.16398,
    k: 0.63297,
};

/// alpha, horizontal
const AH: Fit = Fit {
    a: &[-0.14318, 0.29591, 0.32177, -5.37610, 16.1721],
    b: &[1.82442, 0.77564, 0.63773, -0.96230, -3.29980],
    c: &[-0.55187, 0.19822, 0.13164, 1.47828, 3.43990],
    m: 0.67849,
    k: -1.95537,
};

/// alpha, vertical
const AV: Fit = Fit {
    a: &[-0.07771, 0.56727, -0.20238, -48.2991, 48.5833],
    b: &[2.33840, 0.95545, 1.14520, 0.791669, 0.791459],
    c: &[-0.76284, 0.54039, 0.26809, 0.116226, 0.116479],
    m: -0.053739,
    k: 0.83433,
};

/// dB per km of path in rain of `mm_h` at `mhz`, horizontal and vertical polarisation.
pub fn db_per_km(mhz: f64, mm_h: f64) -> (f64, f64) {
    let ghz = mhz / 1000.0;
    let gamma = |k: &Fit, alpha: &Fit| 10f64.powf(k.at(ghz)) * mm_h.powf(alpha.at(ghz));
    (gamma(&KH, &AH), gamma(&KV, &AV))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrees_with_the_recommendations_table() {
        // k and alpha as Table 5 of P.838-3 lists them, at 10 mm/h
        for (mhz, kh, ah, kv, av) in [
            (4000.0, 0.0001071, 1.6009, 0.0002461, 1.2476),
            (5000.0, 0.0002162, 1.6969, 0.0002428, 1.5317),
            (6000.0, 0.0007056, 1.5900, 0.0004878, 1.5728),
            (8000.0, 0.004115, 1.3905, 0.003450, 1.3797),
        ] {
            let (h, v) = db_per_km(mhz, 10.0);
            let (want_h, want_v) = (kh * 10f64.powf(ah), kv * 10f64.powf(av));
            assert!(
                (h / want_h - 1.0).abs() < 0.005,
                "{mhz} MHz H: {h} vs {want_h}"
            );
            assert!(
                (v / want_v - 1.0).abs() < 0.005,
                "{mhz} MHz V: {v} vs {want_v}"
            );
        }
    }

    #[test]
    fn the_ham_channels_lose_about_twice_what_5_ghz_does() {
        let (h5, _) = db_per_km(5000.0, 10.0);
        let (h, v) = db_per_km(5800.0, 10.0);
        assert!((0.0105..0.011).contains(&h5), "{h5}");
        assert!((0.022..0.024).contains(&h), "{h}");
        assert!((0.015..0.016).contains(&v), "{v}");
        // harder rain costs more than in proportion
        assert!(db_per_km(5800.0, 25.0).0 > 2.5 * h);
    }
}
