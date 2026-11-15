//! Deterministic historical expectancy PNG from one sealed numeric artifact.
//!
//! No candles, outcome evaluation, regression fitting, clock, network or cache.
//! SVG paths/text are rasterized by resvg/tiny-skia with only a bundled font.
//! A call owns one 900x320 RGBA pixmap (1,152,000 bytes), a bounded SVG/series,
//! the renderer's temporary paths/font data, and a <=256-KiB encoded result.
//! All scratch data is dropped before return. The caller owns concurrency,
//! asynchronous scheduling and the immutable per-artifact image cache.
use resvg::{tiny_skia, usvg};
use setups_domain::expectancy::{
    ExpectancyArtifact, ExpectancyError, ExpectancyUnavailableReason, UtcDailyPoint, UtcLinearTrend,
};
use std::{fmt, fmt::Write as _, io};

pub use setups_domain::expectancy::ExpectancyImageKey;

// Keep existing asset keys: claim_render would queue historical PNGs again on a
// version bump. These presentation fixes apply only to future missing renders.
pub const RENDER_VERSION: u32 = 1;
pub const WIDTH: u32 = 900;
pub const HEIGHT: u32 = 320;
pub const MAX_PNG_BYTES: usize = 256 * 1024;
pub const MAX_SVG_BYTES: usize = 768 * 1024;

const FONT: &[u8] = include_bytes!("../assets/DejaVuSans-chart.ttf");
const FONT_FAMILY: &str = "DejaVu Sans";
const TITLE: &str = "Динамика матожидания";
const BACKGROUND: &str = "#121314";
const FOREGROUND: &str = "#eceef0";
const LINE: &str = "#b9bec6";
const POSITIVE: &str = "#18cf98";
const NEGATIVE: &str = "#f08a7f";
const TREND: &str = "#f5b041";
const AXIS: &str = "#a5a8af";
const LEFT: f64 = 80.0;
const RIGHT: f64 = 872.0;
const TOP: f64 = 66.0;
const BOTTOM: f64 = 258.0;
const LABEL_WIDTH: f64 = 112.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    InvalidArtifact(ExpectancyError),
    SeriesUnavailable(ExpectancyUnavailableReason),
    NoFiniteDays,
    InvalidGeometry,
    SvgTooLarge,
    FontUnavailable,
    SvgInvalid,
    AllocationFailed,
    PngEncoding,
    PngTooLarge,
}
impl RenderError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidArtifact(_) => "invalid_expectancy_artifact",
            Self::SeriesUnavailable(_) => "expectancy_series_unavailable",
            Self::NoFiniteDays => "expectancy_no_finite_days",
            Self::InvalidGeometry => "expectancy_chart_invalid_geometry",
            Self::SvgTooLarge => "expectancy_chart_svg_too_large",
            Self::FontUnavailable => "expectancy_chart_font_unavailable",
            Self::SvgInvalid => "expectancy_chart_svg_invalid",
            Self::AllocationFailed => "expectancy_chart_allocation_failed",
            Self::PngEncoding => "expectancy_chart_png_encoding",
            Self::PngTooLarge => "expectancy_chart_png_too_large",
        }
    }
}
impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidArtifact(error) => Some(error),
            _ => None,
        }
    }
}
impl From<fmt::Error> for RenderError {
    fn from(_: fmt::Error) -> Self {
        Self::SvgTooLarge
    }
}

/// The cache key is the domain's immutable source identity plus this renderer's
/// version. It is not a key for mutable "latest study" data or per-signal data.
pub fn image_key(artifact: &ExpectancyArtifact) -> ExpectancyImageKey {
    artifact.image_key(RENDER_VERSION)
}

/// Render from stored UTC points and stored fitted endpoints. Domain validation
/// verifies the sealed artifact; this module never computes a replacement fit.
/// Empty/unavailable/all-null series are explicit errors for text-only fallback.
/// A single finite point is rendered without fabricating a trend.
pub fn render_png(artifact: &ExpectancyArtifact) -> Result<Vec<u8>, RenderError> {
    let svg = render_svg(artifact)?;
    let mut options = usvg::Options {
        font_family: FONT_FAMILY.into(),
        ..Default::default()
    };
    // Do not call load_system_fonts or enable resvg's system-fonts feature.
    // Exactly the same font data and face selection are used on every host.
    options.fontdb_mut().load_font_data(FONT.to_vec());
    if options.fontdb.faces().count() != 1 {
        return Err(RenderError::FontUnavailable);
    }
    let tree = usvg::Tree::from_str(&svg, &options).map_err(|_| RenderError::SvgInvalid)?;
    if tree.size().width() != WIDTH as f32 || tree.size().height() != HEIGHT as f32 {
        return Err(RenderError::InvalidGeometry);
    }
    let mut pixels = tiny_skia::Pixmap::new(WIDTH, HEIGHT).ok_or(RenderError::AllocationFailed)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixels.as_mut(),
    );
    encode_png(pixels.data())
}

struct BoundedSvg(String);
impl fmt::Write for BoundedSvg {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.len() > MAX_SVG_BYTES.saturating_sub(self.0.len()) {
            return Err(fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct Geometry {
    first_x: f64,
    x_span: f64,
    y_min: f64,
    y_max: f64,
}
impl Geometry {
    fn from_points(
        points: &[UtcDailyPoint],
        trend: Option<UtcLinearTrend>,
    ) -> Result<Self, RenderError> {
        let mut minimum = 0.0_f64;
        let mut maximum = 0.0_f64;
        let mut finite = false;
        for point in points {
            if let Some(value) = point.mean_r {
                minimum = minimum.min(value);
                maximum = maximum.max(value);
                finite = true;
            }
        }
        if !finite {
            return Err(RenderError::NoFiniteDays);
        }
        // Match the web plot: fitted endpoints may exceed observed extrema.
        if let Some(trend) = trend {
            minimum = minimum.min(trend.fitted_start_r).min(trend.fitted_end_r);
            maximum = maximum.max(trend.fitted_start_r).max(trend.fitted_end_r);
        }
        let padding = ((maximum - minimum) * 0.08).max(0.1);
        let (y_min, y_max) = if minimum == maximum {
            (minimum - 0.5, maximum + 0.5)
        } else {
            (minimum - padding, maximum + padding)
        };
        let first_x = points.first().ok_or(RenderError::NoFiniteDays)?.x_utc_days;
        let x_span = points.last().ok_or(RenderError::NoFiniteDays)?.x_utc_days - first_x;
        if !y_min.is_finite()
            || !y_max.is_finite()
            || !(y_max - y_min).is_finite()
            || y_max <= y_min
        {
            return Err(RenderError::InvalidGeometry);
        }
        Ok(Self {
            first_x,
            x_span,
            y_min,
            y_max,
        })
    }
    fn x(self, day: f64) -> f64 {
        LEFT + if self.x_span == 0.0 {
            (RIGHT - LEFT) / 2.0
        } else {
            (day - self.first_x) / self.x_span * (RIGHT - LEFT)
        }
    }
    fn y(self, value: f64) -> f64 {
        TOP + (self.y_max - value) / (self.y_max - self.y_min) * (BOTTOM - TOP)
    }
}

fn risk_label(value: f64) -> String {
    // Presentation only: never display a negative zero or a hundreds-character
    // fixed decimal that would clip a legitimate extreme endpoint/axis label.
    let value = if value.abs() < 0.005 { 0.0 } else { value };
    if value.abs() >= 10_000.0 {
        format!("{value:+.2e} R")
    } else {
        format!("{value:+.2} R")
    }
}

fn period_mean_r(artifact: &ExpectancyArtifact, points: &[UtcDailyPoint]) -> f64 {
    if let Some(mean) = artifact
        .selected_cell
        .as_ref()
        .and_then(|cell| cell.raw_r.as_ref())
    {
        return *mean;
    }
    // Presentation fallback from validated sealed points, not a mean of daily
    // means. Normalize weights first to avoid overflowing an intermediate sum.
    let closed = points
        .iter()
        .map(|point| point.tp_count as f64 + point.sl_count as f64)
        .sum::<f64>();
    points
        .iter()
        .filter_map(|point| {
            point
                .mean_r
                .map(|mean| mean * ((point.tp_count as f64 + point.sl_count as f64) / closed))
        })
        .sum()
}

fn render_svg(artifact: &ExpectancyArtifact) -> Result<String, RenderError> {
    artifact.validate().map_err(RenderError::InvalidArtifact)?;
    let points = artifact.daily.as_ref().ok_or_else(|| {
        let setups_domain::expectancy::MetricAvailability::Unavailable { reason } = &artifact.daily
        else {
            unreachable!("available handled above");
        };
        RenderError::SeriesUnavailable(*reason)
    })?;
    let geometry = Geometry::from_points(points, artifact.trend)?;
    let mean = period_mean_r(artifact, points);
    let title_color = if mean > 0.0 {
        POSITIVE
    } else if mean < 0.0 {
        NEGATIVE
    } else {
        FOREGROUND
    };
    let mean_label = risk_label(mean);
    let mut svg = BoundedSvg(String::with_capacity(
        (points.len() * 128 + 8192).min(MAX_SVG_BYTES),
    ));
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="900" height="320" viewBox="0 0 900 320" font-family="{FONT_FAMILY}"><rect width="900" height="320" fill="{BACKGROUND}"/><text id="title" x="450" y="28" text-anchor="middle" font-size="18" fill="{title_color}">{TITLE} · Среднее МО: {mean_label}</text>"#
    )?;
    write!(
        svg,
        r#"<g fill="none" stroke="{AXIS}" stroke-width="1" stroke-opacity="0.4"><path d="M {LEFT} {TOP} V {BOTTOM} H {RIGHT}"/></g><text x="42" y="56" text-anchor="middle" font-size="12" fill="{AXIS}">R</text>"#
    )?;
    let zero = geometry.y(0.0);
    for value in [
        geometry.y_min,
        (geometry.y_min / 2.0 + geometry.y_max / 2.0),
        geometry.y_max,
    ] {
        let y = geometry.y(value);
        if (y - zero).abs() < 14.0 {
            continue; // Reserve the explicit zero label; avoid overlapping ticks.
        }
        write!(
            svg,
            r#"<line x1="{LEFT}" y1="{y:.3}" x2="{RIGHT}" y2="{y:.3}" stroke="{AXIS}" stroke-opacity="0.18"/><text x="70" y="{:.3}" text-anchor="end" font-size="11" fill="{AXIS}">{}</text>"#,
            y + 4.0,
            risk_label(value)
        )?;
    }
    write!(
        svg,
        r#"<line id="zero" x1="{LEFT}" y1="{zero:.3}" x2="{RIGHT}" y2="{zero:.3}" stroke="{AXIS}" stroke-opacity="0.55"/><text id="zero-label" x="70" y="{:.3}" text-anchor="end" font-size="11" fill="{AXIS}">0 R</text>"#,
        zero + 4.0,
    )?;

    // A null represented day ends a segment. Missing calendar dates are NOT
    // inserted as zero/null days; continuous represented finite days connect
    // over their true calendar distance, exactly like the browser model.
    let mut segment_start = 0;
    for end in 0..=points.len() {
        if end == points.len() || points[end].mean_r.is_none() {
            if end.saturating_sub(segment_start) > 1 {
                write!(
                    svg,
                    r#"<polyline class="daily-segment" fill="none" stroke="{LINE}" stroke-width="2" stroke-linejoin="round" points=""#
                )?;
                for point in &points[segment_start..end] {
                    write!(
                        svg,
                        "{:.3},{:.3} ",
                        geometry.x(point.x_utc_days),
                        geometry.y(point.mean_r.expect("finite segment"))
                    )?;
                }
                write!(svg, "\"/>")?;
            }
            segment_start = end + 1;
        }
    }
    if let Some(trend) = artifact.trend {
        write!(
            svg,
            r#"<line id="trend" x1="{:.3}" y1="{:.3}" x2="{:.3}" y2="{:.3}" stroke="{TREND}" stroke-width="2" stroke-dasharray="4 4"/>"#,
            geometry.x(trend.first_x),
            geometry.y(trend.fitted_start_r),
            geometry.x(trend.last_x),
            geometry.y(trend.fitted_end_r)
        )?;
    }
    for point in points {
        if let Some(value) = point.mean_r {
            let fill = if value >= 0.0 { POSITIVE } else { NEGATIVE };
            write!(
                svg,
                r#"<circle class="daily-point" cx="{:.3}" cy="{:.3}" r="4" fill="{fill}"/>"#,
                geometry.x(point.x_utc_days),
                geometry.y(value)
            )?;
        }
    }
    if let Some(trend) = artifact.trend {
        let close = geometry.x(trend.last_x) - geometry.x(trend.first_x) < 2.0 * LABEL_WIDTH;
        trend_label(
            &mut svg,
            "trend-start",
            geometry.x(trend.first_x).clamp(LEFT, RIGHT - LABEL_WIDTH),
            (geometry.y(trend.fitted_start_r) - 12.0).clamp(48.0, BOTTOM - 6.0),
            "start",
            trend.fitted_start_r,
        )?;
        trend_label(
            &mut svg,
            "trend-end",
            geometry.x(trend.last_x).clamp(LEFT + LABEL_WIDTH, RIGHT),
            (geometry.y(trend.fitted_end_r) + if close { 20.0 } else { -12.0 })
                .clamp(48.0, BOTTOM - 6.0),
            "end",
            trend.fitted_end_r,
        )?;
    } else {
        write!(
            svg,
            r#"<text id="no-trend" x="872" y="48" text-anchor="end" font-size="11" fill="{AXIS}">Недостаточно дней для тренда</text>"#
        )?;
    }
    // Keep UTC date labels sparse by pixel spacing, not array index spacing.
    // Reserve the last date; irregular calendar gaps must not produce overlap.
    let mut labels: Vec<usize> = Vec::with_capacity(9);
    let step = points.len().div_ceil(8).max(1);
    for index in (0..points.len()).step_by(step) {
        if labels.last().is_none_or(|last| {
            geometry.x(points[index].x_utc_days) - geometry.x(points[*last].x_utc_days) >= 70.0
        }) {
            labels.push(index);
        }
    }
    let last = points.len() - 1;
    if labels.last() != Some(&last) {
        if labels.len() > 1
            && geometry.x(points[last].x_utc_days)
                - geometry.x(points[*labels.last().expect("first label")].x_utc_days)
                < 70.0
        {
            labels.pop();
        }
        labels.push(last);
    }
    for index in labels {
        let point = &points[index];
        // validate() proves YYYY-MM-DD, independently of host locale/timezone.
        let label = format!("{}.{}", &point.date[8..10], &point.date[5..7]);
        write!(
            svg,
            r#"<text class="date" x="{:.3}" y="283" text-anchor="middle" font-size="11" fill="{AXIS}">{label}</text>"#,
            geometry.x(point.x_utc_days)
        )?;
    }
    write!(
        svg,
        r#"<text x="476" y="308" text-anchor="middle" font-size="11" fill="{AXIS}">Дата сетапа (UTC)</text></svg>"#
    )?;
    Ok(svg.0)
}

fn trend_label(
    svg: &mut BoundedSvg,
    id: &str,
    x: f64,
    y: f64,
    anchor: &str,
    value: f64,
) -> Result<(), RenderError> {
    // Dark halo keeps both endpoint numbers readable over a dense point cloud.
    // At most 13 ASCII glyphs including scientific notation, within the reserved
    // 112-pixel horizontal extent and the image's vertical label margin.
    write!(
        svg,
        r#"<text id="{id}" x="{x:.3}" y="{y:.3}" text-anchor="{anchor}" font-size="12" fill="{TREND}" stroke="{BACKGROUND}" stroke-width="4" stroke-linejoin="round" paint-order="stroke fill">{}</text>"#,
        risk_label(value)
    )?;
    Ok(())
}

struct BoundedPng(Vec<u8>, bool);
impl io::Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PNG_BYTES.saturating_sub(self.0.len()) {
            self.1 = true;
            return Err(io::Error::other("expectancy PNG byte budget exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode_png(rgba: &[u8]) -> Result<Vec<u8>, RenderError> {
    let mut output = BoundedPng(Vec::with_capacity(MAX_PNG_BYTES), false);
    let result = (|| {
        let mut encoder = png::Encoder::new(&mut output, WIDTH, HEIGHT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Best);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
        writer.finish()
    })();
    if output.1 {
        return Err(RenderError::PngTooLarge);
    }
    if result.is_err() {
        return Err(RenderError::PngEncoding);
    }
    if output.0.len() > MAX_PNG_BYTES {
        return Err(RenderError::PngTooLarge);
    }
    Ok(output.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::expectancy::{
        ExpectancyInput, MetricAvailability, UtcDailyInput, build_expectancy_artifact,
    };
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Write};

    const ORIGIN: i64 = 1_767_225_600;
    fn input() -> ExpectancyInput {
        serde_json::from_value(serde_json::json!({
            "source": {
                "studyId":"chart-golden", "derivedSnapshotId":"chart-golden-derived-v2",
                "generationId":"golden-generation", "profileId":"golden-profile",
                "provenance": {"algorithmId":"price_volume_spike", "algorithmVersion":"1.1.1",
                    "optimizerVersion":"fixture-only", "seed":1,"snapshotHash":"frozen-chart-input",
                    "capturedAt":"2026-01-12T00:00:00Z", "fromUnix":ORIGIN,"toUnix":ORIGIN+11*86400,"observationCutoff":ORIGIN+11*86400},
                "direction":"short","tpPct":2.0,"slPct":1.0,
                "analytics":{"clusterEnabled":true,"clusterGapMinutes":480,"timeShiftMinutes":3,"priceChangePct":1.0,"horizonMinutes":10080}
            },
            "selectedCell":{"rawR":0.8,"tpCount":3,"slCount":2}, "matrixMeanR":0.21,
            "daily":{"status":"available","value":[
                {"dayStartUnix":ORIGIN,"rowCount":3,"tpCount":2,"slCount":1,"meanR":1.0},
                {"dayStartUnix":ORIGIN+2*86400,"rowCount":1,"tpCount":0,"slCount":0,"meanR":null},
                {"dayStartUnix":ORIGIN+3*86400,"rowCount":1,"tpCount":0,"slCount":1,"meanR":-1.0},
                {"dayStartUnix":ORIGIN+9*86400,"rowCount":1,"tpCount":1,"slCount":0,"meanR":2.0}
            ]}
        })).unwrap()
    }
    fn fixture() -> ExpectancyArtifact {
        build_expectancy_artifact(input()).unwrap()
    }
    fn with_outcomes(tp_pct: f64, sl_pct: f64, days: &[(i64, u64, u64)]) -> ExpectancyArtifact {
        let mut input = input();
        input.selected_cell = None;
        input.source.tp_pct = tp_pct;
        input.source.sl_pct = sl_pct;
        input.daily = MetricAvailability::Available {
            value: days
                .iter()
                .map(|&(day, tp_count, sl_count)| {
                    let closed = tp_count + sl_count;
                    UtcDailyInput {
                        day_start_unix: ORIGIN + day * 86400,
                        row_count: closed.max(1),
                        tp_count,
                        sl_count,
                        mean_r: (closed > 0).then(|| {
                            (tp_count as f64 * (tp_pct / sl_pct) - sl_count as f64) / closed as f64
                        }),
                    }
                })
                .collect(),
        };
        build_expectancy_artifact(input).unwrap()
    }
    fn decoded(bytes: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).unwrap();
        (info, pixels)
    }
    fn count_color(pixels: &[u8], rgb: [u8; 3], region: (u32, u32, u32, u32)) -> usize {
        let (x1, y1, x2, y2) = region;
        (y1..y2)
            .flat_map(|y| (x1..x2).map(move |x| (y * WIDTH + x) as usize * 4))
            .filter(|index| pixels[*index..*index + 3] == rgb)
            .count()
    }

    #[test]
    fn utc_golden_uses_calendar_spacing_null_breaks_and_saved_fit_labels() {
        let artifact = fixture();
        assert_eq!(artifact.origin_unix, ORIGIN);
        let cell = artifact.selected_cell.as_ref().unwrap();
        assert_eq!((cell.tp_count, cell.sl_count), (3, 2));
        assert_eq!(cell.raw_r.as_ref(), Some(&0.8));
        assert_eq!(cell.win_rate_pct.as_ref(), Some(&60.0));
        let trend = artifact.trend.unwrap();
        assert!((trend.slope_r_per_day - 1.0 / 6.0).abs() < 1e-12);
        let points = artifact.daily.as_ref().unwrap();
        let geometry = Geometry::from_points(points, Some(trend)).unwrap();
        assert!((geometry.x(3.0) - LEFT - (RIGHT - LEFT) / 3.0).abs() < 1e-10);
        let svg = render_svg(&artifact).unwrap();
        assert_eq!(svg.matches("class=\"daily-point\"").count(), 3);
        assert_eq!(svg.matches("class=\"daily-segment\"").count(), 1);
        assert!(svg.contains("+0.00 R</text>"));
        assert!(svg.contains("+1.50 R</text>"));
        assert!(svg.contains(&format!("{TITLE} · Среднее МО: +0.80 R</text>")));
        assert!(svg.contains("Дата сетапа (UTC)"));
        assert!(!svg.contains("Дата сигнала"));
        assert!(svg.contains("03.01</text>")); // represented null day remains on the date axis
        let line = format!(
            "{:.3},{:.3} {:.3},{:.3}",
            geometry.x(3.0),
            geometry.y(-1.0),
            geometry.x(9.0),
            geometry.y(2.0)
        );
        assert!(svg.contains(&line));
        assert_eq!(artifact, fixture()); // renderer did not mutate a sealed artifact
    }

    #[test]
    fn png_dimensions_palette_title_and_protocol_v2_acceptance() {
        let bytes = render_png(&fixture()).unwrap();
        assert!(bytes.len() <= MAX_PNG_BYTES);
        setups_relay_protocol::v2::validate_png(&bytes).unwrap();
        let (info, pixels) = decoded(&bytes);
        assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(&pixels[..4], &[18, 19, 20, 255]);
        let inside_plot = ((100 * WIDTH + 100) * 4) as usize;
        assert_eq!(
            &pixels[inside_plot..inside_plot + 4],
            &[18, 19, 20, 255],
            "open axis path must not fill a black triangle over the background"
        );
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
        assert!(
            count_color(&pixels, [24, 207, 152], (100, 5, 800, 35)) > 100,
            "positive period mean colors the Cyrillic title green"
        );
        assert!(count_color(&pixels, [24, 207, 152], (70, 55, 880, 260)) > 20);
        assert!(count_color(&pixels, [240, 138, 127], (70, 55, 880, 260)) > 10);
        assert!(count_color(&pixels, [245, 176, 65], (70, 40, 880, 260)) > 30);
    }

    #[test]
    fn period_mean_is_trade_weighted_and_zero_label_follows_the_axis_in_every_sign() {
        let weighted = with_outcomes(2.0, 1.0, &[(0, 2, 1), (3, 0, 1), (9, 1, 0)]);
        assert!((period_mean_r(&weighted, weighted.daily.as_ref().unwrap()) - 0.8).abs() < 1e-12);
        assert!(
            render_svg(&weighted)
                .unwrap()
                .contains("Среднее МО: +0.80 R</text>")
        );
        for (artifact, color, rgb, label) in [
            (
                with_outcomes(2.0, 1.0, &[(0, 2, 1)]),
                POSITIVE,
                [24, 207, 152],
                "+1.00 R",
            ),
            (
                with_outcomes(2.0, 1.0, &[(0, 0, 1)]),
                NEGATIVE,
                [240, 138, 127],
                "-1.00 R",
            ),
            (
                with_outcomes(1.0, 1.0, &[(0, 1, 1)]),
                FOREGROUND,
                [236, 238, 240],
                "+0.00 R",
            ),
            (
                with_outcomes(0.999, 1.0, &[(0, 1, 1)]),
                NEGATIVE,
                [240, 138, 127],
                "+0.00 R",
            ),
        ] {
            let svg = render_svg(&artifact).unwrap();
            assert!(svg.contains(&format!(
                "fill=\"{color}\">{TITLE} · Среднее МО: {label}</text>"
            )));
            let geometry =
                Geometry::from_points(artifact.daily.as_ref().unwrap(), artifact.trend).unwrap();
            assert!(svg.contains(&format!(
                "id=\"zero-label\" x=\"70\" y=\"{:.3}\"",
                geometry.y(0.0) + 4.0
            )));
            assert_eq!(svg.matches(">0 R</text>").count(), 1);
            let (_, pixels) = decoded(&render_png(&artifact).unwrap());
            assert!(count_color(&pixels, rgb, (100, 5, 800, 35)) > 100);
        }
    }

    #[test]
    fn outlier_fit_is_included_in_scale_and_both_endpoint_labels_are_visible() {
        let artifact = with_outcomes(10.0, 1.0, &[(0, 1, 10), (1, 1, 0), (2, 1, 0)]);
        let trend = artifact.trend.unwrap();
        assert!(trend.fitted_end_r > 10.0);
        let geometry =
            Geometry::from_points(artifact.daily.as_ref().unwrap(), Some(trend)).unwrap();
        assert!(geometry.y_max > trend.fitted_end_r);
        let svg = render_svg(&artifact).unwrap();
        assert!(svg.contains("id=\"trend-start\"") && svg.contains("id=\"trend-end\""));
        assert!(svg.contains("+1.67 R</text>") && svg.contains("+11.67 R</text>"));
        let bytes = render_png(&artifact).unwrap();
        let (_, pixels) = decoded(&bytes);
        for (x, value) in [
            (LEFT, trend.fitted_start_r),
            (RIGHT - LABEL_WIDTH, trend.fitted_end_r),
        ] {
            let baseline = (geometry.y(value) - 12.0).clamp(48.0, BOTTOM - 6.0);
            assert!(
                count_color(
                    &pixels,
                    [245, 176, 65],
                    (
                        x as u32,
                        (baseline - 14.0) as u32,
                        (x + LABEL_WIDTH) as u32,
                        (baseline + 3.0) as u32
                    )
                ) > 5,
                "endpoint label rasterized within image"
            );
        }
    }

    #[test]
    fn single_point_has_no_invented_fit_and_empty_series_is_explicit() {
        let artifact = with_outcomes(2.0, 1.0, &[(0, 2, 1)]);
        let svg = render_svg(&artifact).unwrap();
        assert!(!svg.contains("id=\"trend\""));
        assert!(svg.contains("id=\"no-trend\""));
        assert!(svg.contains("cx=\"476.000\""));
        setups_relay_protocol::v2::validate_png(&render_png(&artifact).unwrap()).unwrap();
        for artifact in [
            with_outcomes(2.0, 1.0, &[]),
            with_outcomes(2.0, 1.0, &[(0, 0, 0), (2, 0, 0)]),
        ] {
            assert_eq!(render_png(&artifact), Err(RenderError::NoFiniteDays));
        }
        let mut input = input();
        input.daily = MetricAvailability::Unavailable {
            reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
        };
        let unavailable = build_expectancy_artifact(input).unwrap();
        assert_eq!(
            render_png(&unavailable),
            Err(RenderError::SeriesUnavailable(
                ExpectancyUnavailableReason::MissingFrozenOutcomes
            ))
        );
    }

    #[test]
    fn same_artifact_has_same_png_hash_and_source_key_is_versioned() {
        let artifact = fixture();
        let first = render_png(&artifact).unwrap();
        let second = render_png(&artifact).unwrap();
        assert_eq!(Sha256::digest(&first), Sha256::digest(&second));
        let key = image_key(&artifact);
        assert_eq!(
            key.render_version, 1,
            "do not requeue historical assets for presentation-only fixes"
        );
        assert_eq!(key, artifact.image_key(RENDER_VERSION));
        assert_eq!(
            key.timezone,
            setups_domain::expectancy::ExpectancyTimezone::UTC
        );
        let mut changed = artifact;
        changed.source.derived_snapshot_id = "new-sealed-copy".into();
        assert_ne!(image_key(&changed), key);
        assert_eq!(risk_label(-1e-12), "+0.00 R");
    }

    #[test]
    fn byte_budgets_and_invalid_sealed_artifacts_fail_before_render() {
        let mut output = BoundedPng(Vec::new(), false);
        output.write_all(&vec![0; MAX_PNG_BYTES]).unwrap();
        assert!(output.write_all(b"x").is_err());
        assert_eq!(output.0.len(), MAX_PNG_BYTES);
        let mut svg = BoundedSvg(String::new());
        assert!(fmt::Write::write_str(&mut svg, &"x".repeat(MAX_SVG_BYTES)).is_ok());
        assert!(fmt::Write::write_str(&mut svg, "x").is_err());
        let mut artifact = fixture();
        artifact.trend.as_mut().unwrap().fitted_end_r = 999.0;
        assert_eq!(
            render_png(&artifact),
            Err(RenderError::InvalidArtifact(
                ExpectancyError::InconsistentArtifact
            ))
        );
        assert!(FONT.len() < 1_000_000);
    }

    #[test]
    fn maximum_series_stays_within_svg_pixel_and_png_budgets() {
        let mut input = input();
        input.selected_cell = None;
        input.source.tp_pct = 10.0;
        input.source.sl_pct = 1.0;
        input.source.provenance.from_unix = ORIGIN + 1;
        input.source.provenance.to_unix = ORIGIN + 1 + 3660 * 86400;
        input.source.provenance.observation_cutoff = input.source.provenance.to_unix;
        input.daily = MetricAvailability::Available {
            value: (0..=3660)
                .map(|day| {
                    let tp_count = ((day * 7919) % 101) as u64;
                    let sl_count = 100 - tp_count;
                    UtcDailyInput {
                        day_start_unix: ORIGIN + day * 86400,
                        row_count: 100,
                        tp_count,
                        sl_count,
                        mean_r: Some((tp_count as f64 * 10.0 - sl_count as f64) / 100.0),
                    }
                })
                .collect(),
        };
        let artifact = build_expectancy_artifact(input).unwrap();
        assert!(render_svg(&artifact).unwrap().len() <= MAX_SVG_BYTES);
        let bytes = render_png(&artifact).unwrap();
        assert!(bytes.len() <= MAX_PNG_BYTES);
        setups_relay_protocol::v2::validate_png(&bytes).unwrap();
    }

    #[test]
    #[ignore = "writes portable temporary PNG/SVG samples for visual review"]
    fn write_visual_samples() {
        let directory = std::env::temp_dir().join("expectancy-chart-samples");
        std::fs::create_dir_all(&directory).unwrap();
        for (name, artifact) in [
            ("utc-january-golden", fixture()),
            (
                "outlier-endpoint-labels",
                with_outcomes(10.0, 1.0, &[(0, 1, 10), (1, 1, 0), (2, 1, 0)]),
            ),
            ("single-day", with_outcomes(2.0, 1.0, &[(0, 2, 1)])),
        ] {
            std::fs::write(
                directory.join(format!("{name}.png")),
                render_png(&artifact).unwrap(),
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{name}.svg")),
                render_svg(&artifact).unwrap(),
            )
            .unwrap();
        }
        eprintln!("expectancy_chart_samples={}", directory.display());
    }
}
