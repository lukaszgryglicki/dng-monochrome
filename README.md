# dng-monochrome

Develop monochrome Leica DNG photographs into **16-bit grayscale PNG** and
**8-bit grayscale JPEG**, without demosaicing or reducing the raw image to
an embedded preview.

PNG is always lossless, grayscale, 16 bits per pixel, and encoded at the
encoder's **maximum compression setting: DEFLATE level 9 with adaptive row
filtering**. It is never reduced to 8-bit, RGB, or an indexed palette.
JPEG uses the same developed image, quantized according to its selected
transfer: linear by default, or sRGB when requested.

**Defaults:** photographic optimization **on**, clipping strength **3**,
linear PNG (`gAMA=1`), and linear grayscale JPEG quality **90**.
Standard photographic EXIF is copied to both
formats, including ISO, exposure time and the recorded aperture/lens metadata.

## Build

Rust 1.89 or newer and a native linker are required. The decoder, math parser,
processing, PNG encoder, and JPEG encoder are Rust code. No LibRaw, dcraw,
ImageMagick, or external image-conversion process is required.

```sh
make                         # tests, then stripped release build
./target/release/dng-monochrome -help
```

| Target | Result |
| --- | --- |
| `make`, `make all` | Tests and release build |
| `make build`, `make release` | Optimized, stripped `target/release/dng-monochrome` |
| `make debug` | `target/debug/dng-monochrome` |
| `make test` | Synthetic decoder, numerical, expression, and CLI tests |
| `make test-real DNG_MONO_SAMPLE=/path/shot.DNG` | Opt-in original-Leica decoder and full conversion tests |
| `make lint` | Formatting check and Clippy with warnings treated as errors |
| `make fmt` | Format Rust source |
| `make static` | Stripped static executable under `target/static/<target>/release/` |
| `make clean` | Remove Cargo build artifacts, not input or output photographs |

Both BSD make and GNU make can run these targets. Builds use four jobs by
default (`JOBS=8` overrides this); tests use four concurrent test threads
(`TEST_THREADS=2` overrides this). These limits do not constrain the converter's
automatic CPU detection.

On FreeBSD, `make static` uses the native target with static CRT linking.
On GNU/Linux it selects the corresponding musl target, which must already be
installed, for example:

```sh
rustup target add x86_64-unknown-linux-musl
make static
# Override target selection when needed; cross-compilation also needs a linker:
make static STATIC_TARGET=x86_64-unknown-linux-musl
```

The static target checks that the resulting executable really is statically
linked. It never installs toolchains or changes a shared compiler automatically.

## Usage

```sh
# One PNG/JPEG pair per file; the shell expands *.DNG.
dng-monochrome *.DNG

# Put xyz.png and xyz.jpg beside xyz.DNG in the working directory.
dng-monochrome -o . *.DNG

# Recursively process a directory, preserving its relative subdirectories.
dng-monochrome /path/to/photos -o /path/to/dng-mono

# Produce both versions with one raw decode per input, plus detailed reports.
dng-monochrome /path/to/photos -o /path/to/dng-mono --both --report

# Independent dark/highlight percentages, measured against the full histogram.
dng-monochrome -dark 0.8 -light 1.2% -o adjusted *.DNG

# Exact minimum-to-maximum stretch, without automatic tail clipping.
dng-monochrome --no-optimize --dark 0 --light 0 shot.DNG

# Manual tone mapping; single-dash and double-dash long flags both work.
dng-monochrome -func 'x^.5' -o bright shot.DNG
dng-monochrome -func 'sin(x*pi)' -func-scale -o mapped shot.DNG

# Keep the linear look, lifting only values below 0.35 by a moderate amount.
dng-monochrome -tone-band 0:0.35:0.4 -o shadow-lift shot.DNG

# Photographic optimization is enabled by default; -best is still accepted.
dng-monochrome -best -o best *.DNG
dng-monochrome -no-optimize -o unoptimized *.DNG

# Adjust automatic clipping without choosing fixed manual percentages.
dng-monochrome -clip-strength 1 -o conservative *.DNG
dng-monochrome -clip-strength 9 -best -o stronger *.DNG

# Linear PNG without tail clipping for further numerical processing or editing.
dng-monochrome --no-optimize --dark 0 --light 0 -o linear shot.DNG

# Display-encoded PNG for viewers that ignore linear PNG gamma.
dng-monochrome --transfer srgb -o display shot.DNG

# Explicitly select the defaults: linear-quantized JPEG and linear PNG.
dng-monochrome --transfer linear -o both-linear shot.DNG

# Use sRGB JPEG for sharing; explicit per-format values override --transfer.
dng-monochrome --png-transfer linear --jpeg-transfer srgb shot.DNG
dng-monochrome --jpg-transfer linear -o linear-jpeg shot.DNG

# Detailed diagnostics, or silent conversion (errors remain visible).
dng-monochrome -debug shot.DNG
dng-monochrome -silent -best -o quiet shot.DNG

# Analysis only: one JSON object per input on standard output.
dng-monochrome --analyze /path/to/photos > analysis.jsonl
```

The default output directory is `./dng-mono`. An input named `xyz.DNG` or
`xyz.dng` produces `xyz.png` and `xyz.jpg`; `--report` adds `xyz.json`.
With `--both`, the relative names are placed under `auto/` and `best/`.
Multiple directory arguments get a directory-name prefix to keep their trees
separate. Repeated identical inputs are deduplicated. Conflicting output names
are rejected before any conversion; specify separate output directories or
pass their parent directories instead.

Directory traversal does not follow symlinks. Explicit file arguments can
resolve through symlinks. Use `--` before a filename beginning with a dash.
Original DNG files are opened read-only and never modified.

### All command-line options

The documented `--long` options also accept `-long`, including `-help`,
`-best`, `-dark=0.8`, and `-func-scale`.

| Option | Meaning / default |
| --- | --- |
| `DNG_OR_DIRECTORY ...` | One or more monochrome DNG files or recursively searched directories |
| `-o, --output DIR` | Output root, default `dng-mono`; alias `--output-dir` |
| `--dark PERCENT` | Override darkest-pixel clipping; automatic if omitted |
| `--light PERCENT` | Override lightest-pixel clipping; automatic if omitted |
| `--clip-strength 1..9` | Automatic clipping strength; default **3**. Level 1 is conservative; level 9 aims around 1-2% per end |
| `--func EXPR` | Quoted real-valued function of normalized lightness `x` |
| `--tone-band START:END:AMOUNT` | Lift only the selected `[0,1]` intensity band; repeat for separate bands. Shorthand for `--func`, mutually exclusive with it; default off |
| `--func-clip` | Clamp finite function results to `[0,1]`; default function policy |
| `--func-scale` | Rescale the minimum and maximum results actually present in this image to `[0,1]` |
| `--func-wrap` | Wrap finite out-of-range results modulo one; leave existing `[0,1]` values unchanged |
| `--optimize, --best` | Explicitly enable photographic optimization, which is already **on by default** |
| `--no-optimize` | Skip photographic optimization; retain range selection and any custom function |
| `--both` | Generate unoptimized `auto/` and optimized `best/` versions; conflicts with explicit optimization toggles |
| `--transfer srgb\|linear` | Set **both** formats; unspecified means the separate defaults below |
| `--png-transfer srgb\|linear` | PNG encoding, default **`linear`**; overrides `--transfer` |
| `--jpeg-transfer srgb\|linear` | JPEG encoding, default **`linear`**; alias `--jpg-transfer`; overrides `--transfer` |
| `--jpeg-quality 1..100` | JPEG quality; default `90` for sharing; PNG is the lossless master |
| `--threads 0..256` | Processing workers; `0` detects available hardware parallelism |
| `--no-crop` | Retain the full raw raster instead of the recommended DNG crop |
| `--report` | Write per-pair JSON with settings, thresholds, clipping and precision statistics |
| `--analyze` | Analyze only, creating no output files; cannot combine with tone/functions, `--both`, reports or overwrite |
| `--overwrite` | Explicitly permit replacement of existing output files |
| `-v, --verbose, --debug` | Detailed per-image metadata, noise-model evidence and range diagnostics |
| `-q, --silent` | Suppress progress, warnings, best mappings and completion; errors and explicit `--analyze` JSON remain |
| `-h, --help` | Usage; `--help` or `-help` includes full descriptions and examples |
| `-V, --version` | Version |
| `--param-NAME VALUE` | Override a tuning parameter listed below; `--param-NAME=VALUE` and `-param-NAME VALUE` also work |

Percentages can have an optional `%` suffix. Each must be finite and in
`[0,100)`, and their sum must be less than 100. Either end can be overridden
independently. Function policies are mutually exclusive and require `--func`
or `--tone-band`.
Verbose/debug and silent are mutually exclusive. Manual percentages always
override their own end, irrespective of clipping strength.

No lower PNG compression or bit-depth option exists. Maximum compression can
cost noticeably more CPU time on large, noisy images.

### Advanced `--param-*` tuning

All tunable constants in this application's processing, estimation, expression
limits, progress formatting and JPEG Huffman encoding are exposed below.
**With no overrides, the algorithms and PNG pixels are unchanged; the only
changed rendering default is JPEG transfer from sRGB to linear.**
Existing controls such as `--clip-strength`, `--jpeg-quality`, `--threads`,
manual percentages and transfers retain their existing names.

Each option accepts `--param-name=value`, `--param-name value`,
`-param-name=value`, or `-param-name value`. Values are numbers except the
explicit `true`/`false` Huffman switch. All numeric bounds below are inclusive;
integer counts must be integers. NaN, infinity, out-of-range values and invalid
parameter combinations are errors before input files are opened.
Fractions use `0..1`, not percent, unless a parameter explicitly says percent.

Mathematical/format definitions are deliberately fixed: normalized endpoints,
16-/8-bit ranges, the exact 65,536-bin raw histogram, standard sRGB/APEX
equations, Gaussian/MAD and quantization conversion factors, median/regression
definitions, SNR1/SNR3 report meanings, parser precedence, TIFF/EXIF tags and
size limits, and the mandatory maximum-compression grayscale16 PNG format.
Dependency internals and hard resource/numerical safety guards are not tuning
options. Changing a binomial smoothing radius or noise patch size derives its
kernel, buffer sizes and degrees of freedom consistently.

The effective values are included in `--analyze`, `--verbose` and `--report`
JSON under `parameters`, using snake_case field names. Noise/clipping overrides
can change the DR estimate; artistic tone overrides do not change raw analysis.
Aggressive noise-model settings do not make the estimate calibrated or more
accurate merely because they produce a larger number.

#### Histogram scheduling

| Option | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `--param-histogram-chunks-per-thread` | `4` | 1..64 | Target histogram chunks per worker |
| `--param-histogram-min-chunk` | `262144` | 1024..16777216 | Minimum pixels per chunk |

#### Clipping and coherent detail

| Option | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `--param-auto-cap` | `0.02` | 0..0.49 | Conservative tail-search cap, as a fraction |
| `--param-tail-steps` | `200` | 2..65536 | Tail-search subdivisions |
| `--param-tail-noise-sigma` | `2` | 0..100 | Minimum tail span in noise sigmas |
| `--param-tail-min-span` | `4` | 1e-6..65535 | Minimum tail span in raw codes |
| `--param-tail-knee-score` | `0.2` | 0..1 | Minimum normalized knee score |
| `--param-dark-snr` | `2` | 0..100 | SNR for the automatic dark floor; the one-code minimum remains |
| `--param-strength-target-base` | `1` | 0..49 | Strength-9 target base, percent |
| `--param-strength-knee-weight` | `0.5` | 0..10 | Weight multiplying knee percent |
| `--param-strength-target-min` | `1` | 0..49 | Minimum strength-9 target, percent |
| `--param-strength-target-max` | `2` | 0..49 | Maximum strength-9 target, percent |
| `--param-tied-span` | `1` | 1e-6..65535 | Raw-code interval retained at ties, limited to the prior span |
| `--param-noise-warning-sigmas` | `8` | 0..1000 | Warn below this selected-span/shadow-sigma ratio |
| `--param-coherent-samples` | `500000` | 1..10000000 | Approximate coherent-tail sample-position budget |
| `--param-coherent-radius` | `1` | 1..8 | Neighborhood radius; side is `2*radius+1` |
| `--param-coherent-neighbors` | `5` | 1..288 | Required same-tail neighbors, excluding center |
| `--param-coherent-min-support` | `4` | 1..1000000 | Supported positions needed before expanding a bound |
| `--param-coherent-tail-fraction` | `0.005` | 0..0.5 | Lower supported-extreme quantile; upper is `1-value` |
| `--param-coherent-noise-margin` | `2` | 0..100 | Noise-sigma margin around supported extremes |

The strength target is
`max(existing_percent, clamp(base + knee_weight*knee_percent, min, max))`.
The existing strength 1..9 interpolation and manual-end precedence are unchanged.
The minimum target must not exceed the maximum. Required neighbors must fit
the selected neighborhood: at radius 1 there are only 8, at radius 2 there are 24.
Change the neighbor requirement explicitly when changing the radius.

#### Noise estimation and confidence

| Option | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `--param-noise-patch-side` | `16` | 5..128 | Square patch side, pixels |
| `--param-noise-max-patches` | `12000` | 1..1000000 | Approximate spatial sampling budget |
| `--param-noise-haar-lag-small` | `1` | 1..127 | Small Haar separation, pixels |
| `--param-noise-haar-lag-medium` | `2` | 1..127 | Medium Haar separation |
| `--param-noise-haar-lag-large` | `4` | 1..127 | Large Haar separation |
| `--param-noise-texture-max` | `1.5` | 0.01..100 | Maximum plane-residual/large-lag noise ratio |
| `--param-noise-lag-growth-max` | `1.35` | 0.01..100 | Maximum large/medium Haar ratio |
| `--param-noise-min-patches` | `32` | 1..1000000 | Minimum candidates and minimum tail-group size |
| `--param-noise-patches-per-bin` | `48` | 1..1000000 | Target patches per brightness group |
| `--param-noise-max-bins` | `16` | 3..128 | Maximum brightness groups |
| `--param-noise-variance-quantile` | `0.25` | 0.05..0.95 | Variance-envelope quantile; bias correction follows it |
| `--param-noise-min-sigma` | `0.5` | 1e-6..65535 | Minimum resolved spatial sigma, raw codes |
| `--param-noise-fit-min-bins` | `6` | 3..128 | Minimum groups required for a fitted model |
| `--param-noise-fit-min-span` | `16` | 1e-6..65535 | Minimum brightness span, raw codes |
| `--param-noise-fit-pair-separation` | `0.15` | 0..0.99 | Minimum pair separation as a fraction of brightness span |
| `--param-noise-fit-iterations` | `4` | 0..64 | Robust weighted regression iterations |
| `--param-noise-fit-residual-scale` | `0.15` | 1e-6..10 | Relative-residual downweighting scale |
| `--param-noise-fit-negative-slope` | `0.15` | 0..10 | Tolerated negative slope times span / median variance |
| `--param-noise-fit-max-error` | `0.25` | 0..10 | Maximum median relative model error |
| `--param-noise-refinement-passes` | `3` | 0..32 | Variance-normalized brightness-bin refinements |
| `--param-noise-fit-success-fraction` | `0.8` | 0.01..1 | Required successful leave-one-bin-out fraction |
| `--param-noise-sensitivity-tail` | `0.1` | 0..0.5 | Lower sensitivity quantile; upper is `1-value` |
| `--param-noise-read-ratio-max` | `3` | 1..100 | Maximum upper/lower read-noise sensitivity ratio |
| `--param-noise-extrapolation-fraction` | `0.35` | 0..100 | Darkest-bin signal limit / fitted brightness span |
| `--param-noise-shadow-read-sigmas` | `6` | 0..1000 | Alternative darkest-bin limit in read sigmas |
| `--param-noise-tail-divisor` | `5` | 1..1024 | Candidate count divided by this gives tail-group size |
| `--param-noise-correlation-min-variance` | `0.25` | 0..4294836225 | Minimum variance for correlation diagnostics |
| `--param-noise-correlation-warning` | `1.3` | 0..1000 | Correlation ratio above which to emit a note |
| `--param-noise-profile-tolerance` | `2` | 1..1000 | Maximum observed/profile sigma ratio or its reciprocal |

Haar separations must satisfy `small < medium < large < patch_side`.
The fit's minimum bin count must not exceed the maximum bin count.
Fewer available patches or failed evidence gates still produce the documented
explicit proxy/unavailable result, not an invented fitted read-noise value.
The variance-quantile correction assumes Gaussian sampling; changing it is an
experiment, not a replacement for calibration.

#### Photographic optimization

| Option | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `--param-optimize-strength` | `1` | 0..1 | Blend the complete optimized curve with range-only light |
| `--param-tone-exposure-bias` | `0` | -16..16 | Exposure compensation in stops after automatic selection |
| `--param-tone-min-exposure-ev` | `-2` | -16..16 | Minimum searched exposure, stops |
| `--param-tone-max-exposure-ev` | `6` | -16..16 | Maximum searched exposure, stops |
| `--param-tone-exposure-steps` | `8` | 1..64 | Search candidates per stop |
| `--param-tone-midtone-quantile` | `0.5` | 0..1 | Input quantile treated as the midtone |
| `--param-tone-shadow-quantile` | `0.1` | 0..1 | Input shadow quantile for contrast scoring |
| `--param-tone-highlight-quantile` | `0.9` | 0..1 | Input highlight quantile for contrast scoring |
| `--param-tone-target-midtone` | `0.43` | 0..1 | Target perceptual midtone |
| `--param-tone-midtone-tolerance` | `0.15` | 1e-6..1 | Midtone objective normalization |
| `--param-tone-target-contrast` | `0.6` | 0..1 | Desired perceptual shadow/highlight separation |
| `--param-tone-contrast-tolerance` | `0.4` | 1e-6..1 | Contrast objective normalization |
| `--param-tone-contrast-weight` | `0.15` | 0..1000 | Exposure-search contrast penalty weight |
| `--param-tone-noise-threshold` | `0.1` | 0..1 | Noise threshold before penalizing exposure |
| `--param-tone-noise-tolerance` | `0.1` | 1e-6..1 | Noise objective normalization |
| `--param-tone-noise-weight` | `0.35` | 0..1000 | Exposure-search noise penalty weight |
| `--param-tone-contrast-strength` | `0.75` | 0..1 | Histogram-contrast blend, independent of exposure |
| `--param-tone-bins` | `1024` | 16..65536 | Perceptual histogram bins, not raw histogram bins |
| `--param-tone-smoothing-passes` | `3` | 0..64 | Binomial histogram smoothing passes |
| `--param-tone-smoothing-radius` | `2` | 0..16 | Binomial smoothing radius in bins |
| `--param-tone-density-prior` | `0.15` | 1e-6..100 | Positive uniform density prior |
| `--param-tone-density-power` | `0.5` | 0..4 | Normalized density exponent; default is square root |
| `--param-tone-noise-target` | `0.04` | 1e-6..1 | Noise budget controlling contrast-gain caps |
| `--param-tone-cap-min` | `1.15` | 1..64 | Minimum noise-dependent upper slope cap |
| `--param-tone-cap-max` | `3` | 1..64 | Maximum upper slope cap |
| `--param-tone-slope-min` | `0.35` | 0..1 | Lower density slope before blending |
| `--param-tone-solver-upper` | `32` | 1e-6..1e9 | Upper normalization-scale search bound |
| `--param-tone-solver-iterations` | `48` | 1..128 | Normalization bisection iterations |

Exposure bounds must be ordered; equal bounds select a fixed exposure before
applying the bias. Shadow, midtone and highlight quantiles must be ordered.
The minimum cap must not exceed the maximum. `solver_upper * density_prior`
must be at least 1 so the normalization root is bracketed.
Insufficient solver convergence is an explicit error; increase iterations or
reduce the upper bound rather than accepting an invalid/non-monotonic curve.

Useful controls for ordinary editing:

```sh
# Weaken the WHOLE optimized rendering, including exposure.
dng-monochrome --param-optimize-strength=0.6 shot.DNG

# Add half a stop after automatic exposure selection.
dng-monochrome -param-tone-exposure-bias 0.5 shot.DNG

# Brighter target midtone, but gentler histogram contrast.
dng-monochrome --param-tone-target-midtone=0.5 --param-tone-contrast-strength=0.4 shot.DNG

# Fixed +1-stop exposure shoulder, without histogram contrast.
dng-monochrome --param-tone-min-exposure-ev=1 --param-tone-max-exposure-ev=1 \
  --param-tone-contrast-strength=0 shot.DNG

# Penalize visible noise more and lower the allowed contrast noise budget.
dng-monochrome --param-tone-noise-weight=0.7 --param-tone-noise-target=0.025 shot.DNG
```

For full optimized curve `F(x)` and amount `s`, the result before `--func` is
`(1-s)*x + s*F(x)`. Amount 0 bypasses the complete optimization, matching
`--no-optimize` pixels; it still retains the selected `best` mode/report label.
Contrast strength 0 disables only histogram contrast, retaining the exposure
shoulder. Exposure bias and all other optimizer controls have no pixel effect
with `--no-optimize`, amount 0, or a constant image. None of these controls adds
local processing, sharpening or denoising.

#### Expression limits, progress and encoding

| Option | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `--param-expression-max-bytes` | `16384` | 1..1048576 | Maximum expression source bytes |
| `--param-expression-max-parentheses` | `128` | 1..512 | Maximum nested parentheses |
| `--param-expression-max-depth` | `256` | 1..512 | Maximum recursive parsing depth |
| `--param-mapping-steps` | `20` | 2..1000 | Unit interval divisions; print the `steps-1` interior samples |
| `--param-mapping-input-decimals` | `2` | 0..12 | Printed mapping input precision |
| `--param-mapping-output-decimals` | `5` | 0..17 | Printed mapping output precision |
| `--param-progress-dr-decimals` | `1` | 0..12 | DR and code-span precision |
| `--param-progress-range-decimals` | `1` | 0..12 | Raw endpoint precision |
| `--param-progress-clip-decimals` | `3` | 0..12 | Actual clipping percentage precision |
| `--param-progress-elapsed-decimals` | `1` | 0..6 | Elapsed-time precision |
| `--param-progress-aperture-decimals` | `2` | 0..12 | Displayed f-number precision |
| `--param-jpeg-optimize-huffman` | `true` | true/false | Optimize entropy tables; decoded JPEG pixels are unchanged |

Parser limits are independent; whichever is reached first applies. Raising
limits does not change the grammar or allow nonfinite results. When requesting
more mapping samples, increase input decimals if necessary to distinguish
their printed positions. Progress precision never changes image pixels,
EXIF rationals or JSON numeric precision.

## What happens to the pixels

1. Decode the raw integer samples, including lossless JPEG compression. There
   is no demosaicing, white balance, color matrix, or embedded-preview conversion.
   Apply DNG linearization when supplied by the decoder. Apply the recommended
   crop (or active area when no crop is supplied) and physically apply EXIF
   orientation. `--no-crop` disables only cropping.
2. Build an exact **65,536-bin histogram** of the retained raw pixels. All
   integer codes remain available; there is no preliminary 8-bit image.
3. Select lower and upper endpoints, automatically or from the requested
   full-histogram percentiles.
4. Map `x = clamp((raw - lower) / (upper - lower), 0, 1)`.
5. Apply the monotonic photographic optimization curve unless `--no-optimize`
   is set (or this is the `auto/` half of `--both`).
6. If requested, evaluate `--func` and apply its clip/scale/wrap policy.
7. Encode the result using the selected PNG transfer and round to `0..65535`.
   Produce JPEG samples from that final PNG representation using the selected
   JPEG transfer.

All calculations after decoding use `f64`. Each expression is evaluated once
per occupied raw code, not once per pixel, and cached in a lookup table.
Histogram construction, noise sampling, pixel mapping and supported raw
decompression use Rayon. PNG and JPEG encoding run concurrently. Files are
processed sequentially to avoid keeping many 18/36/60-megapixel images in RAM.

Large, Medium and Small are not special-cased: dimensions and precision come
from DNG metadata. A 16-bit container does **not** imply 16 bits of original
signal. The supplied 5280x3506 M11 Monochrom files have `WhiteLevel=16383`,
a **14-bit ceiling**, and a 5272x3498 recommended crop. The converter preserves
their low-order samples; it does not assume that Small means 12-bit.

### Automatic useful-range estimation

A single photograph cannot unambiguously distinguish dark subject detail from
read noise, or a small bright object from an outlier. **This is a conservative
useful-range heuristic, not a measurement of physical sensor dynamic range or
effective ADC bits.** Full-spectrum conversion does not change that limitation.

The conservative **strength-1** algorithm, with default parameters:

- Sample approximately 12,000 or fewer 16x16 patches, remove each local plane,
  and estimate residual noise using Gaussian-scaled median absolute
  deviations. Compare Haar residuals at separations 1, 2 and 4 pixels.
  Reject structured patches and patches whose residuals keep growing with
  scale; correlated noise must reach a plateau. A patch touching raw zero or
  the white limit is excluded as censored. Ordinary below-black samples are
  **not** excluded: read noise legitimately extends below the black offset.
- Group weak-texture patches by brightness. Use a sampling-bias-corrected
  lower-quartile noise envelope, variance-normalized refinement and robust
  inverse-variance regression to fit `variance(signal) = a*signal + b`.
  Require enough patches, brightness coverage, fit quality and a stable
  positive read-noise intercept. If these requirements fail, explicitly use
  measured shadow noise as a conservative proxy rather than inventing a
  zero read-noise floor. A valid DNG `NoiseProfile`, when present, is converted
  from normalized units and checked against spatial observations.
- Inspect each histogram tail independently, from 0 to 2%, in 0.01-percentage
  point steps. A knee is the largest departure of normalized quantile distance
  from a straight line. Accept it only when that departure is at least 0.2
  and the tail spans at least four raw codes and twice the corresponding
  estimated noise sigma. Uniform tails need not be clipped at all.
- Protect spatially coherent extreme detail: sample 3x3 neighborhoods around
  tail pixels, require at least five of eight neighbors in the same tail,
  and expand endpoints toward robust supported extremes plus a noise margin.
  Small genuine bright/dark regions are not automatically treated as bad
  pixels. This is why strength 1 can select extremely small clipping fractions.
- On the dark side also consider the estimated SNR-2 signal floor above
  `BlackLevel`, bounded by the 2% quantile. The proxy uses `2*shadow_sigma`.
  On the bright side use the sparse-tail knee and spatial evidence, not a
  blanket assumption that all highlights are noise.
- Respect the camera black/white calibration in automatic mode. Additional
  heuristic clipping is capped at 2% per end; samples outside the physical
  calibration can account for more than that and are reported separately.

**Strengths 2-9** deliberately relax conservative detail preservation to
improve visible contrast. Let `p1` be the actual strength-1 clipping percentage
at an automatic end and `k` its detected histogram-knee percentage (0-2).
The strength-9 target is `p9 = max(p1, clamp(1 + k/2, 1, 2))`. At strength `s`,
the requested percentile is `p1 + (s-1)/8 * (p9-p1)`. Thus level 9 normally
approaches **1-2%**, not 9% or 10%; default level **3** is a moderate compromise.
No existing calibrated/conservative clipping is undone. Ties can make actual
clipping smaller; a tied interval is bounded with an explicit warning rather
than collapsed. These higher levels are intentional contrast trimming, **not
proof that every discarded pixel was noise**. Use manual percentages for
exact control, or level 1 to favor detail preservation.

Texture, sharpening in the source, correlated/binning noise, hot pixels and
scene content can bias the spatial estimate. Saturated highlights and genuine
black regions can be indistinguishable from unwanted tails. Nothing can restore
detail already clipped by the camera. Inspect both versions and use manual
percentages, particularly `--dark 0 --light 0`, when preservation is paramount.
This tool does not spatially denoise, sharpen, or create missing tonal detail.

Manual clipping uses the **full cropped-image histogram**, before either tail
is removed and without automatically clamping that overridden end to camera
black/white levels. For `N` pixels it discards `floor(N * percent / 100)` ranks
from the chosen end. Equal-valued pixels cannot be partly discarded by a
global value mapping, so actual strictly-outside counts may be lower; reports
show both requested and actual percentages. Discarded pixels become black or
white, not holes in the image.

Constant images use the metadata black/white interval and emit a warning;
there is no legitimate contrast to invent. Collapsed automatic endpoints fall
back, with a warning, to the calibrated observed interval. A collapsed manually
selected interval is an error. Very low signal-to-estimated-noise spans also
produce a warning.

### `--optimize` / `--best`

This is a reproducible global photographic rendering, not a claim that one
curve is aesthetically best for every scene. Maximizing variance alone tends
to crush intermediate tones and amplify noise, so that is not the objective.
It is enabled by default; use `--no-optimize` for range-only development.
The numbers below describe the unchanged default algorithm; the
`--param-tone-*` controls above expose its tuning choices.

First search exposure factors `a` from 0.25 to 64 in eighth-stop increments.
Balance a display midtone near 0.43, the 10th-to-90th-percentile contrast and
predicted visible noise. The endpoint-preserving exposure shoulder is:

```text
y = a*x / (1 + (a-1)*x)
```

Then build a 1,024-bin perceptual histogram, excluding already-clipped pixels.
Smooth it, take a square-root density with a positive uniform prior, and
allocate contrast with bounded slopes. Noise-dependent upper bounds prevent
blindly expanding noisy peaks. Blend the resulting cumulative curve with
identity at strength 0.75, then convert back to linear light. Perceptual
contrast slopes stay between 0.5125 and 2.5; noisy regions have tighter caps.
This is not unrestricted histogram equalization or mere variance maximization.

The whole mapping is global, monotonic, continuous and endpoint-preserving.
There is no local tone-map halo, sharpening or spatial denoising. Constant
images are not given artificial contrast. Reports contain exposure, predicted
midtone noise and all curve control points. Deliberately low-key/high-key
photographs may still be better without this mode.

Every successful optimized conversion prints `0.05->...`, `0.10->...`, through
`0.95->...` (19 samples, five output decimals). The fixed identity endpoints
0 and 1 are omitted. These are the best curve's
**normalized linear** input/output values, before `--func` and the output
transfer, not raw codes or sRGB byte values. `--silent` suppresses this line.

### Expressions

`x` is **linear normalized light**, after range selection and optional
optimization, but **before** either output transfer. Thus `x^2` darkens,
`x^.5` / `sqrt(x)` lifts shadows, and `x/2` halves linear intensity.
PNG is linear by default, so its numbers directly represent the processed
intensities. Add `--no-optimize` when the function should act on only the
range-normalized input rather than on the default photographic rendering.

The parser supports:

| Category | Syntax |
| --- | --- |
| Arithmetic | `+`, `-`, `*`, `/`, `%` (remainder), `^` (power), unary `+`/`-` |
| Grouping/numbers | Nested `()`, `.5`, `0.5`, `1e-3`, `2.5E+1` |
| Constants | `pi`, `e` |
| Elementary | `sqrt`, `abs`, `exp`, `ln`, `log` (natural logarithm), `log2`, `log10` |
| Stable variants | `log1p(x)=ln(1+x)`, `expm1(x)=exp(x)-1`, `exp2` |
| Trigonometry | `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2(y,x)` |
| Hyperbolic | `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh` |
| Rounding/sign | `floor`, `ceil`, `round`, `sign`, `signum` |
| Multiple arguments | `min(a,b,...)`, `max(a,b,...)`, `pow(a,b)`, `hypot(a,b)`, `clamp(x,lo,hi)` |
| sRGB tone helpers | `srgb(x)` (exact standard encoding curve), `srgb_band(x,start,end,amount)` (anchored regional blend) |

Trigonometric arguments are radians. Powers associate to the right:
`2^3^2=512`; powers bind more tightly than unary minus: `-x^2=-(x^2)`.
Multiplication/division/remainder, then addition/subtraction, associate left.
Multiplication must be explicit (`2*x`, not `2x`). Names are case-sensitive.
`sign(0)=0`; `signum` uses Rust's sign-bit convention (`signum(+0)=1`,
`signum(-0)=-1`). `min`/`max` need at least one argument.

For example:

```sh
dng-monochrome \
  -func '((exp(2*x)-1)/(exp(2)-1)+(sin(pi*x)^2)*(1-x)/4+((1-(1-x)^3)-x)/8)' \
  -func-scale -o custom *.DNG
```

Only finite final function results are accepted. `sqrt(-1)`, division by zero
and exponential overflow produce explicit errors, not silently black pixels.
Clipping/wrapping cannot repair NaN or infinity. A function is checked only at
intensities actually represented in the image; a singularity at an absent code
does not matter. `--func-scale` likewise uses only actual result values, and
rejects constant results rather than inventing contrast.

`--func-wrap` leaves `0` and `1` unchanged; outside the range it uses Euclidean
modulo one, e.g. `-0.2 -> 0.8`, `1.2 -> 0.2`, and `2 -> 0`.
It is an artistic, generally non-monotonic option, not a recovery technique.

The math-only parser has no scripting, files, network, variables other than
`x`, or complex arithmetic. Default limits are 16,384 input bytes, 128 nested
parentheses and 256 nested parsing operations; evaluation uses a flat reusable
stack, so long chains do not recursively evaluate an AST.

### Between the linear and sRGB looks

Use **`--tone-band 0:0.35:0.4`** as a starting point for more shadow detail
without brightening the whole image. The three numbers are **start, end and
amount**, each in `[0,1]`, with `start < end`. They refer to normalized lightness
after range selection and any optimization, **not percentages of pixels**.
Nothing changes when this option is absent.

```sh
# Lift shadows below 0.35; leave every value at/above 0.35 unchanged.
dng-monochrome shot.DNG -tone-band 0:0.35:0.4 -o lifted --report

# Also lift highlights, independently and more gently; keep 0.35..0.7 unchanged.
dng-monochrome shot.DNG -tone-band 0:0.35:0.4 -tone-band 0.7:1:0.2 -o two-bands

# Whole-range halfway blend of the linear and sRGB curves.
dng-monochrome shot.DNG -tone-band 0:1:0.5 -o halfway

# Equivalent function, or explicitly combine the lift with other adjustments.
dng-monochrome shot.DNG -func 'srgb_band(x,0,0.35,0.4)' -o lifted
dng-monochrome shot.DNG -func 'srgb_band(x^1.1,0,0.35,0.4)' -o custom
dng-monochrome shot.DNG -func '0.6*x+0.4*srgb(x)' -o global-blend
```

Amount `0` is exactly unchanged. Amount `1` uses the full sRGB **shape rescaled
into that band**; for the whole range `0:1:1`, its sample values match the full
sRGB curve. Within a partial band `[L,U]`, the mapping is:

```text
u = (x-L)/(U-L)
target = L + (U-L)*srgb(u)
y = (1-amount)*x + amount*target
```

The band endpoints stay fixed and values outside stay unchanged. This avoids
the brightness jump and tone reversal that a hard cutoff of global `srgb(x)`
would cause. The joins are continuous, though their slopes need not match.
For `0:0.35:0.4`, `0.05` becomes about `0.088`, `0.10` becomes `0.140`, and
`0.35` and all brighter values stay as they were. Increase the end to reach
more midtones, or increase the amount for stronger lift. Shadow noise becomes
more visible too; no missing sensor detail is recovered.

Repeat `--tone-band` for non-overlapping ranges; touching endpoints and either
argument order are allowed. It is a shortcut for the equivalent nested
`srgb_band` expression saved in the JSON report. To combine with `--func`,
write those calls inside the expression rather than supplying both options.
The usual expression limits and policies apply. Leave the default clipping
policy for unchanged exterior values: `--func-scale` deliberately rescales the
whole result and may change those values.

These are **tone adjustments, not a new file encoding**. The examples use the
default linear PNG/JPEG output. Adding an sRGB output transfer encodes the
already-adjusted values again, so it is not the same halfway-look recipe.
PNG/EXIF encoding tags remain accurate. Use `--no-optimize` if the curve should
act on only range-normalized input; otherwise it adjusts the default optimized
rendering. Different viewers' color management can still change appearance.

### PNG and JPEG appearance

Default PNG pixels store **linear intensities with `gAMA=1.0`**, preserving the
selected numerical data without an extra display-transfer quantization.
`--png-transfer srgb` instead uses the standard sRGB transfer and an `sRGB`
chunk. The shared `--transfer` option changes both formats; `--png-transfer`
and `--jpeg-transfer` override it regardless of argument order.
A color-managed viewer can display either correctly; a viewer that ignores
linear PNG gamma may show the default version too dark. Select
`--jpeg-transfer srgb` for conventional display-encoded JPEG sharing.

JPEG is native single-channel grayscale and defaults to linear. When both
transfers match, its samples are `round(PNG16 / 257)`. For linear PNG / sRGB
JPEG, the final PNG intensities are first sRGB-encoded; for sRGB PNG / linear
JPEG, they are first sRGB-decoded. They are then rounded to eight bits.
Thus JPEG always approximates the final quantized master, not an unrelated
intermediate image. The default
JPEG quality is 90 with optimized Huffman tables. Quality 100 is still a
lossy JPEG, not a lossless substitute for PNG.

The default `--jpeg-transfer linear` (or `--jpg-transfer linear`) stores
linear-quantized bytes. This may resemble a linear PNG in an unmanaged viewer
such as a particular Geeqie configuration, but appearance depends on color
management. Linear JPEG has coarser shadow gradation than sRGB JPEG; choose
`--jpeg-transfer srgb` when finer shadow quantization is wanted for sharing.
Linear JPEG is marked with EXIF
Gamma=1 and uncalibrated colorspace; many viewers still assume sRGB.
Reports expose `png_transfer` and `jpeg_transfer`; the legacy `transfer` key
also records the PNG transfer.

Stretching a 14-bit range into 16-bit output does not create two new bits of
information. Reports distinguish container precision, white-level bits,
occupied codes, retained code-span bits, histogram entropy and final PNG code
counts. Default clipping deliberately discards tails; `--best`, expressions
and integer rounding also transform values. Lossless PNG compression does not
make those edits lossless relative to the original DNG. Keep the originals.

### Dynamic-range reports

Console `approx DR ... bits/stops` is the **noise-limited SNR-1 estimate**,
with confidence, not the raw-code span. Each doubling is one stop. Reports
also distinguish SNR-3, selected scene contrast and raw-code-span bits
(`log2(upper-lower+1)`).

For a supported variance model, the signal needed for SNR `t` is
`s_t = (t*t*a + sqrt(t^4*a*a + 4*t*t*b))/2`. Estimate available DR from the
selected highlight signal above the camera black offset divided by `s_t`.
The integer-code threshold is at least one raw code. When read-noise
extrapolation is unsupported, use `t*measured_shadow_sigma` instead and mark
the result **low confidence**; this can underestimate DR because shot noise
and texture are included. Insufficient evidence is explicitly unavailable.

`fit_sensitivity_stops` describes leave-one-brightness-bin-out fit sensitivity,
**not** a calibrated statistical confidence interval. Single-image texture,
fixed-pattern noise, temperature, processing and missing dark/flat calibration
remain limitations even with a stable fit. ISO is reported correctly using
extended 32-bit tags, but is **not** used to manufacture a DR number. Filters
cannot be reliably classified from monochrome pixels.

The older `estimated_snr_span_stops` report field is the simpler selected-span
over measured-shadow-sigma proxy, retained separately for comparison. Neither
metric is guaranteed sensor DR or ADC ENOB. For example, the supplied ISO
160000 images use 16-bit containers with a 14-bit white level but only about
five estimated SNR-1 stops at the recorded exposure, not 14 noise-free bits.

Method background: [single-image Poisson-Gaussian noise modeling](https://webpages.tuni.fi/foi/papers/Foi-PoissonianGaussianClippedRaw-2007-IEEE_TIP.pdf)
and [image-sensor noise measurement limitations](https://www.imatest.com/imaging/image-sensor-noise/).

### Photographic EXIF

PNG carries standard `eXIf` metadata; JPEG carries an EXIF APP1 segment.
The converter copies recognized standard photographic fields: extended ISO,
exposure time, FNumber when present, ApertureValue, focal length, lens
information, capture times, exposure compensation, photographer/copyright and
GPS when present. Original rational exposure/aperture values are preserved,
not recalculated from a lens menu selection. Orientation becomes 1 because
pixels were physically oriented; dimensions, sample depth, colorspace and
processing software describe the output.

Normal progress includes ISO, aperture and the original rational exposure
time. If standard FNumber is absent, an available APEX ApertureValue is shown
as its f-number equivalent with an `(APEX)` label; missing values are explicit.
This does not correct an inaccurate lens-menu selection.

Raw-only calibration, thumbnails, XMP and proprietary MakerNotes are not
copied blindly: their offsets and raw-development settings may be invalid in
a PNG/JPEG. Some Leica files expose a proprietary MakerNote `FNumber=1`
placeholder despite having no standard FNumber; the actual standard
ApertureValue is preserved, and the tool does not invent or "correct" an
FNumber from that placeholder. Some viewers display only the legacy 16-bit
ISO field (65535); the true ISO160000 is retained in the extended tags.

GPS/owner metadata can reveal location/identity when sharing. It is retained
as requested, not anonymized. Viewers vary in PNG EXIF support; tools such as
ExifTool can inspect it. Oversized photographic EXIF that cannot fit JPEG's
APP1 limit causes an explicit error before either companion is published.

## Output safety and failures

Existing outputs are never overwritten without `--overwrite`. Symlink outputs
and non-regular destinations are rejected. All companions are encoded into
temporary files before publication, and each file is atomically published.
The complete PNG/JPEG/JSON set is not a filesystem transaction: if publication
of a later companion fails, earlier published files remain and the command
reports the failure. Failed staging cleans up temporary files.

When overwriting images that already have a JSON sidecar, include `--report`
so it cannot silently become stale. Progress/warnings/errors go to standard
error; the completion line or `--analyze` JSON goes to standard output.
Batch conversion continues after individual input/variant failures and exits
nonzero if any failed. Syntax errors exit with code 2; processing errors with
code 1; successful conversion/help/version with code 0.

The intended input is monochrome integer DNG. Color/CFA samples, unsupported
floating-point DNGs or non-uniform black-level layouts are rejected explicitly.
The raw decoder does not reproduce a proprietary Leica rendering or apply
every DNG opcode/profile. It is not a substitute for dark/flat calibration of
a modified full-spectrum sensor.

## Tests

Synthetic DNG fixtures are generated in temporary directories: packed 12/14/16
bits, both TIFF endiannesses, crop/orientation, calibration, malformed input,
and actual CLI conversion. Tests decode the produced PNG and JPEG to check
their real bit depth, grayscale shape, transfer metadata, pixel values, JPEG
approximation and maximum-compression zlib header. Independent TIFF-byte
checks cover embedded EXIF, exact ISO160000/exposure/aperture/lens/date values,
source endianness, oriented/cropped geometry and metadata overflow failures.

Numerical coverage includes all 65,536 raw codes, known Gaussian and
signal-dependent noise, correlation, texture, censoring/below-black samples,
calibrated profiles, independent manual percentiles, coherent/isolated tails,
all clipping strengths, tied/constant/extreme images, positive-slope
photographic curves, noise-gain limits and deterministic one/multi-thread results. Expression
coverage includes independent formula comparisons, every documented function,
over 5,000-character formulas, 100 nested parentheses, precedence, all boundary
policies, overflow/domain failures and 4,000 arbitrary-input parser cases.
CLI tests cover every flag, shell-expanded multi-file input, recursion, both
variants, collisions, overwrite protection, reports, and invalid combinations.
The default 19-point best mapping, optimization/opt-out, all transfer pairs,
per-format override precedence, ISO/f/t display and silent/debug behavior are
also checked. Parameter tests cover every option's implicit/explicit defaults,
both CLI spellings, nondefault values, invalid values/combinations, report and
processing paths, unchanged default pixel fingerprints, variable noise geometry
and bias correction, optimizer amount/exposure/contrast, parser limits and
Huffman-only encoding changes. README defaults are checked against the actual
CLI declarations. To fully decode and check a generated collection:

```sh
DNG_MONO_OUTPUTS=/path/to/dng-mono \
  cargo test --test collection -- --ignored --nocapture
```

Private photographs are not bundled. Set `DNG_MONO_SAMPLE` for the optional
original-Leica tests; they require a representative M11 Monochrom 14-bit file
with more than 4,096 distinct codes. Optional local fixtures can live under
the ignored `tests/fixtures/local/` directory.

## License

This application's source is licensed under [Apache-2.0](LICENSE).
Dependencies retain their respective licenses. In particular,
[`rawler`](https://github.com/dnglab/dnglab) is LGPL-2.1; distributing linked
binaries, especially static binaries, requires observing its corresponding
source, notices and relinking obligations. The application's Apache license
does not relicense that dependency. `Cargo.lock` pins the dependency versions.
