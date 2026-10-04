use anyhow::{Result, ensure};
use clap::Args;
use serde::Serialize;

macro_rules! parameters {
    ($($heading:literal { $(
        $field:ident: $type:ty = $default:expr, $long:literal, $min:expr, $max:expr, $help:literal;
    )* })*) => {
        #[derive(Clone, Debug, PartialEq, Args, Serialize)]
        #[group(skip)]
        pub struct Parameters {
            $($(
                #[arg(long = $long, default_value_t = $default, allow_hyphen_values = true,
                    help_heading = $heading,
                    help = concat!($help, " [", stringify!($min), "..", stringify!($max), "]"))]
                pub $field: $type,
            )*)*
            #[arg(long = "param-jpeg-optimize-huffman", default_value_t = true,
                action = clap::ArgAction::Set, help_heading = "Encoding parameters",
                help = "Optimize JPEG Huffman tables (true/false); does not change decoded pixels")]
            pub jpeg_optimize_huffman: bool,
        }

        impl Default for Parameters {
            fn default() -> Self {
                Self {
                    $($($field: $default,)*)*
                    jpeg_optimize_huffman: true,
                }
            }
        }

        impl Parameters {
            fn validate_scalars(&self) -> Result<()> {
                $($(
                    let value = self.$field as f64;
                    ensure!(
                        value.is_finite() && ($min as f64..=$max as f64).contains(&value),
                        "--{} must be finite and in [{}, {}]", $long, $min, $max
                    );
                )*)*
                Ok(())
            }
        }
    };
}

parameters! {
    "Histogram parameters" {
        histogram_chunks_per_thread: usize = 4, "param-histogram-chunks-per-thread", 1, 64, "Target histogram chunks per worker";
        histogram_min_chunk: usize = 262144, "param-histogram-min-chunk", 1024, 16777216, "Minimum pixels per histogram chunk";
    }
    "Range parameters" {
        auto_cap: f64 = 0.02, "param-auto-cap", 0.0, 0.49, "Conservative automatic tail cap as a fraction, not percent";
        tail_steps: usize = 200, "param-tail-steps", 2, 65536, "Subdivisions of the conservative tail search";
        tail_noise_sigma: f64 = 2.0, "param-tail-noise-sigma", 0.0, 100.0, "Minimum tail width in noise sigmas";
        tail_min_span: f64 = 4.0, "param-tail-min-span", 0.000001, 65535.0, "Minimum tail width in raw codes";
        tail_knee_score: f64 = 0.2, "param-tail-knee-score", 0.0, 1.0, "Minimum normalized histogram-knee score";
        dark_snr: f64 = 2.0, "param-dark-snr", 0.0, 100.0, "SNR used for the automatic dark floor";
        strength_target_base: f64 = 1.0, "param-strength-target-base", 0.0, 49.0, "Strength-9 target base, in percent";
        strength_knee_weight: f64 = 0.5, "param-strength-knee-weight", 0.0, 10.0, "Weight of knee percent in the strength-9 target";
        strength_target_min: f64 = 1.0, "param-strength-target-min", 0.0, 49.0, "Minimum strength-9 target, in percent";
        strength_target_max: f64 = 2.0, "param-strength-target-max", 0.0, 49.0, "Maximum strength-9 target, in percent";
        tied_span: f64 = 1.0, "param-tied-span", 0.000001, 65535.0, "Raw-code interval retained when stronger clipping reaches ties";
        noise_warning_sigmas: f64 = 8.0, "param-noise-warning-sigmas", 0.0, 1000.0, "Warn when selected span is below this many shadow sigmas";
        coherent_samples: usize = 500000, "param-coherent-samples", 1, 10000000, "Approximate maximum coherent-tail sample positions";
        coherent_radius: usize = 1, "param-coherent-radius", 1, 8, "Coherent-tail neighborhood radius in pixels";
        coherent_neighbors: usize = 5, "param-coherent-neighbors", 1, 288, "Required same-tail neighbors, excluding the center";
        coherent_min_support: usize = 4, "param-coherent-min-support", 1, 1000000, "Minimum supported positions before protecting a tail";
        coherent_tail_fraction: f64 = 0.005, "param-coherent-tail-fraction", 0.0, 0.5, "Robust supported-extreme quantile; upper tail uses one minus this";
        coherent_noise_margin: f64 = 2.0, "param-coherent-noise-margin", 0.0, 100.0, "Noise-sigma margin around supported extremes";
    }
    "Noise parameters" {
        noise_patch_side: usize = 16, "param-noise-patch-side", 5, 128, "Square patch side in pixels";
        noise_max_patches: usize = 12000, "param-noise-max-patches", 1, 1000000, "Approximate maximum noise patches";
        noise_haar_lag_small: usize = 1, "param-noise-haar-lag-small", 1, 127, "Small Haar separation in pixels";
        noise_haar_lag_medium: usize = 2, "param-noise-haar-lag-medium", 1, 127, "Medium Haar separation in pixels";
        noise_haar_lag_large: usize = 4, "param-noise-haar-lag-large", 1, 127, "Large Haar separation in pixels";
        noise_texture_max: f64 = 1.5, "param-noise-texture-max", 0.01, 100.0, "Maximum plane-residual/large-lag noise ratio for weak texture";
        noise_lag_growth_max: f64 = 1.35, "param-noise-lag-growth-max", 0.01, 100.0, "Maximum large/medium Haar noise growth";
        noise_min_patches: usize = 32, "param-noise-min-patches", 1, 1000000, "Minimum candidate patches and minimum shadow/highlight group";
        noise_patches_per_bin: usize = 48, "param-noise-patches-per-bin", 1, 1000000, "Target patches per brightness bin";
        noise_max_bins: usize = 16, "param-noise-max-bins", 3, 128, "Maximum brightness bins";
        noise_variance_quantile: f64 = 0.25, "param-noise-variance-quantile", 0.05, 0.95, "Variance-envelope quantile, with matching Gaussian bias correction";
        noise_min_sigma: f64 = 0.5, "param-noise-min-sigma", 0.000001, 65535.0, "Minimum resolved spatial sigma in raw codes";
        noise_fit_min_bins: usize = 6, "param-noise-fit-min-bins", 3, 128, "Minimum bins for variance-model fitting";
        noise_fit_min_span: f64 = 16.0, "param-noise-fit-min-span", 0.000001, 65535.0, "Minimum fit brightness span in raw codes";
        noise_fit_pair_separation: f64 = 0.15, "param-noise-fit-pair-separation", 0.0, 0.99, "Minimum pair separation as a fraction of fit brightness span";
        noise_fit_iterations: usize = 4, "param-noise-fit-iterations", 0, 64, "Robust weighted regression iterations";
        noise_fit_residual_scale: f64 = 0.15, "param-noise-fit-residual-scale", 0.000001, 10.0, "Relative-residual downweighting scale";
        noise_fit_negative_slope: f64 = 0.15, "param-noise-fit-negative-slope", 0.0, 10.0, "Allowed negative slope times span, relative to median variance";
        noise_fit_max_error: f64 = 0.25, "param-noise-fit-max-error", 0.0, 10.0, "Maximum median relative fit error";
        noise_refinement_passes: usize = 3, "param-noise-refinement-passes", 0, 32, "Variance-normalized bin refinement passes";
        noise_fit_success_fraction: f64 = 0.8, "param-noise-fit-success-fraction", 0.01, 1.0, "Required fraction of successful leave-one-bin-out fits";
        noise_sensitivity_tail: f64 = 0.1, "param-noise-sensitivity-tail", 0.0, 0.5, "Lower read-noise sensitivity quantile; upper is one minus this";
        noise_read_ratio_max: f64 = 3.0, "param-noise-read-ratio-max", 1.0, 100.0, "Maximum upper/lower read-noise sensitivity ratio";
        noise_extrapolation_fraction: f64 = 0.35, "param-noise-extrapolation-fraction", 0.0, 100.0, "Maximum darkest-bin signal relative to fitted signal span";
        noise_shadow_read_sigmas: f64 = 6.0, "param-noise-shadow-read-sigmas", 0.0, 1000.0, "Alternative darkest-bin limit in fitted read-noise sigmas";
        noise_tail_divisor: usize = 5, "param-noise-tail-divisor", 1, 1024, "Divide candidate count by this for shadow/highlight groups";
        noise_correlation_min_variance: f64 = 0.25, "param-noise-correlation-min-variance", 0.0, 4294836225.0, "Minimum patch variance included in correlation diagnostics";
        noise_correlation_warning: f64 = 1.3, "param-noise-correlation-warning", 0.0, 1000.0, "Correlation ratio above which to emit a note";
        noise_profile_tolerance: f64 = 2.0, "param-noise-profile-tolerance", 1.0, 1000.0, "Maximum measured/profile sigma ratio or its reciprocal";
    }
    "Optimization parameters" {
        optimize_strength: f64 = 1.0, "param-optimize-strength", 0.0, 1.0, "Blend the complete optimized result with range-only light; zero bypasses optimization";
        tone_exposure_bias: f64 = 0.0, "param-tone-exposure-bias", -16.0, 16.0, "Exposure compensation in stops after automatic exposure selection";
        tone_min_exposure_ev: f64 = -2.0, "param-tone-min-exposure-ev", -16.0, 16.0, "Minimum searched exposure in stops; equal bounds select a fixed exposure";
        tone_max_exposure_ev: f64 = 6.0, "param-tone-max-exposure-ev", -16.0, 16.0, "Maximum searched exposure in stops";
        tone_exposure_steps: usize = 8, "param-tone-exposure-steps", 1, 64, "Exposure candidates per stop";
        tone_midtone_quantile: f64 = 0.5, "param-tone-midtone-quantile", 0.0, 1.0, "Input quantile used as the exposure midtone";
        tone_shadow_quantile: f64 = 0.1, "param-tone-shadow-quantile", 0.0, 1.0, "Input shadow quantile used to score contrast";
        tone_highlight_quantile: f64 = 0.9, "param-tone-highlight-quantile", 0.0, 1.0, "Input highlight quantile used to score contrast";
        tone_target_midtone: f64 = 0.43, "param-tone-target-midtone", 0.0, 1.0, "Target perceptual midtone";
        tone_midtone_tolerance: f64 = 0.15, "param-tone-midtone-tolerance", 0.000001, 1.0, "Midtone objective normalization";
        tone_target_contrast: f64 = 0.6, "param-tone-target-contrast", 0.0, 1.0, "Desired perceptual shadow-to-highlight contrast";
        tone_contrast_tolerance: f64 = 0.4, "param-tone-contrast-tolerance", 0.000001, 1.0, "Contrast objective normalization";
        tone_contrast_weight: f64 = 0.15, "param-tone-contrast-weight", 0.0, 1000.0, "Contrast penalty weight in exposure selection";
        tone_noise_threshold: f64 = 0.10, "param-tone-noise-threshold", 0.0, 1.0, "Perceptual noise level before exposure is penalized";
        tone_noise_tolerance: f64 = 0.10, "param-tone-noise-tolerance", 0.000001, 1.0, "Noise objective normalization";
        tone_noise_weight: f64 = 0.35, "param-tone-noise-weight", 0.0, 1000.0, "Noise penalty weight in exposure selection";
        tone_contrast_strength: f64 = 0.75, "param-tone-contrast-strength", 0.0, 1.0, "Perceptual histogram-contrast blend after exposure";
        tone_bins: usize = 1024, "param-tone-bins", 16, 65536, "Perceptual histogram bins";
        tone_smoothing_passes: usize = 3, "param-tone-smoothing-passes", 0, 64, "Binomial histogram smoothing passes";
        tone_smoothing_radius: usize = 2, "param-tone-smoothing-radius", 0, 16, "Binomial histogram smoothing radius in bins";
        tone_density_prior: f64 = 0.15, "param-tone-density-prior", 0.000001, 100.0, "Positive uniform density prior";
        tone_density_power: f64 = 0.5, "param-tone-density-power", 0.0, 4.0, "Normalized histogram density exponent";
        tone_noise_target: f64 = 0.04, "param-tone-noise-target", 0.000001, 1.0, "Noise target used to cap perceptual contrast gain";
        tone_cap_min: f64 = 1.15, "param-tone-cap-min", 1.0, 64.0, "Minimum noise-dependent upper slope cap";
        tone_cap_max: f64 = 3.0, "param-tone-cap-max", 1.0, 64.0, "Maximum perceptual upper slope cap";
        tone_slope_min: f64 = 0.35, "param-tone-slope-min", 0.0, 1.0, "Minimum perceptual density slope before blending";
        tone_solver_upper: f64 = 32.0, "param-tone-solver-upper", 0.000001, 1000000000.0, "Upper density-scale search bound; multiplied by prior must be at least one";
        tone_solver_iterations: usize = 48, "param-tone-solver-iterations", 1, 128, "Density normalization bisection iterations; insufficient convergence is an error";
    }
    "Expression parameters" {
        expression_max_bytes: usize = 16384, "param-expression-max-bytes", 1, 1048576, "Maximum expression source bytes";
        expression_max_parentheses: usize = 128, "param-expression-max-parentheses", 1, 512, "Maximum nested parentheses";
        expression_max_depth: usize = 256, "param-expression-max-depth", 1, 512, "Maximum recursive parsing depth";
    }
    "Progress parameters" {
        mapping_steps: usize = 20, "param-mapping-steps", 2, 1000, "Divide the unit interval into this many parts and print interior best samples";
        mapping_input_decimals: usize = 2, "param-mapping-input-decimals", 0, 12, "Best mapping input decimal places";
        mapping_output_decimals: usize = 5, "param-mapping-output-decimals", 0, 17, "Best mapping output decimal places";
        progress_dr_decimals: usize = 1, "param-progress-dr-decimals", 0, 12, "Dynamic-range and code-span decimal places";
        progress_range_decimals: usize = 1, "param-progress-range-decimals", 0, 12, "Selected raw endpoint decimal places";
        progress_clip_decimals: usize = 3, "param-progress-clip-decimals", 0, 12, "Actual clipping percentage decimal places";
        progress_elapsed_decimals: usize = 1, "param-progress-elapsed-decimals", 0, 6, "Elapsed-time decimal places";
        progress_aperture_decimals: usize = 2, "param-progress-aperture-decimals", 0, 12, "Displayed aperture decimal places";
    }
}

impl Parameters {
    pub fn validate(&self) -> Result<()> {
        self.validate_scalars()?;
        ensure!(
            self.strength_target_min <= self.strength_target_max,
            "--param-strength-target-min must not exceed --param-strength-target-max"
        );
        let neighbors = (2 * self.coherent_radius + 1).pow(2) - 1;
        ensure!(
            self.coherent_neighbors <= neighbors,
            "--param-coherent-neighbors exceeds the {neighbors} neighbors at --param-coherent-radius {}",
            self.coherent_radius
        );
        ensure!(
            self.noise_haar_lag_small < self.noise_haar_lag_medium
                && self.noise_haar_lag_medium < self.noise_haar_lag_large
                && self.noise_haar_lag_large < self.noise_patch_side,
            "--param-noise-haar-lag-small < medium < large < --param-noise-patch-side is required"
        );
        ensure!(
            self.noise_fit_min_bins <= self.noise_max_bins,
            "--param-noise-fit-min-bins must not exceed --param-noise-max-bins"
        );
        ensure!(
            self.tone_min_exposure_ev <= self.tone_max_exposure_ev,
            "--param-tone-min-exposure-ev must not exceed --param-tone-max-exposure-ev"
        );
        ensure!(
            self.tone_shadow_quantile <= self.tone_midtone_quantile
                && self.tone_midtone_quantile <= self.tone_highlight_quantile,
            "--param-tone-shadow-quantile <= midtone quantile <= highlight quantile is required"
        );
        ensure!(
            self.tone_cap_min <= self.tone_cap_max,
            "--param-tone-cap-min must not exceed --param-tone-cap-max"
        );
        ensure!(
            self.tone_solver_upper * self.tone_density_prior >= 1.0,
            "--param-tone-solver-upper * --param-tone-density-prior must be at least one to bracket normalization"
        );
        Ok(())
    }
}
