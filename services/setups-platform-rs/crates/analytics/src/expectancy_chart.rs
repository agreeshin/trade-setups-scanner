//! Deterministic historical expectancy PNG from one sealed numeric artifact.
//!
//! No candles, outcome evaluation, clock, network or cache. A bounded display
//! fit uses sealed daily points and the original study's immutable trend spec;
//! it never replaces the artifact's saved numerical OLS or recommendation.
//! SVG paths/text are rasterized by resvg/tiny-skia with only a bundled font.
//! A call owns one 900x320 RGBA pixmap (1,152,000 bytes), a bounded SVG/series,
//! the renderer's temporary paths/font data, and a <=256-KiB encoded result.
//! All scratch data is dropped before return. The caller owns concurrency,
//! asynchronous scheduling and the immutable per-artifact image cache.
use resvg::{tiny_skia, usvg};
use setups_domain::expectancy::{
    ExpectancyArtifact, ExpectancyError, ExpectancyObjective, ExpectancyUnavailableReason,
    UtcDailyPoint,
};
use setups_domain::forecast::{
    ForecastError, ForecastRecipe, ForecastUnavailableReason, ProbabilityForecast,
};
#[cfg(test)]
use setups_domain::forecast::{
    ProbabilityCurveRequest, ProbabilityDay, evaluate_probability, probability_curve,
};
use setups_domain::trend::{
    TrendBendDays, TrendBlendLambda, TrendError, TrendInput, TrendMode, fit_weighted_trend,
};
use std::{fmt, fmt::Write as _, io};

pub use setups_domain::expectancy::ExpectancyImageKey;

// Keep existing asset keys: claim_render would queue historical PNGs again on a
// version bump. These presentation fixes apply only to future missing renders.
pub const RENDER_VERSION: u32 = 1;
/// Explicit opt-in only. Never pass this to the legacy missing-asset scanner:
/// existing study PNGs and queued delivery bodies retain their original bytes.
pub const PROBABILITY_RENDER_VERSION: u32 = 2;
/// New objective-specific overlays only, never the legacy asset scanner.
pub const OBJECTIVE_RENDER_VERSION: u32 = 3;
pub const WIDTH: u32 = 900;
pub const HEIGHT: u32 = 320;
pub const MAX_PNG_BYTES: usize = 256 * 1024;
pub const MAX_SVG_BYTES: usize = 768 * 1024;

const FONT: &[u8] = include_bytes!("../assets/DejaVuSans-chart.ttf");
const FONT_FAMILY: &str = "DejaVu Sans";
const TITLE: &str = "Динамика матожидания алгоритма";
const BACKGROUND: &str = "#121314";
const FOREGROUND: &str = "#eceef0";
const LINE: &str = "#b9bec6";
const POSITIVE: &str = "#18cf98";
const NEGATIVE: &str = "#f08a7f";
const TREND: &str = "#f5b041";
const AXIS: &str = "#a5a8af";
const GRID: &str = "#2e3238";
const LEFT: f64 = 80.0;
const RIGHT: f64 = 872.0;
const TOP: f64 = 66.0;
const BOTTOM: f64 = 258.0;
const LABEL_WIDTH: f64 = 112.0;

/// Only the three immutable study fields needed by this render. Missing legacy
/// fields use study defaults; invalid present values fail rather than guessing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StudyTrend {
    pub trend_mode: TrendMode,
    pub trend_bend_days: TrendBendDays,
    pub trend_blend_lambda: TrendBlendLambda,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    InvalidArtifact(ExpectancyError),
    InvalidTrend(TrendError),
    InvalidForecast(ForecastError),
    ForecastUnavailable(ForecastUnavailableReason),
    ForecastMismatch,
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
            Self::InvalidTrend(_) => "expectancy_chart_invalid_trend",
            Self::InvalidForecast(_) => "expectancy_chart_invalid_forecast",
            Self::ForecastUnavailable(_) => "expectancy_chart_forecast_unavailable",
            Self::ForecastMismatch => "expectancy_chart_forecast_mismatch",
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
            Self::InvalidTrend(error) => Some(error),
            Self::InvalidForecast(error) => Some(error),
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

/// Render with the legacy study defaults. Production supplies the saved study
/// fields explicitly through render_png_with_trend, never mutable UI settings.
/// Empty/unavailable/all-null series are explicit errors for text-only fallback.
/// A single finite point is rendered without fabricating a trend.
pub fn render_png(artifact: &ExpectancyArtifact) -> Result<Vec<u8>, RenderError> {
    render_png_with_trend(artifact, StudyTrend::default())
}

pub fn render_png_with_trend(
    artifact: &ExpectancyArtifact,
    study: StudyTrend,
) -> Result<Vec<u8>, RenderError> {
    let svg = render_svg_with_trend(artifact, study)?;
    rasterize_svg(&svg)
}

/// A scalar adapter for the NEW explicit probability recipe. It deliberately
/// does not use rowCount, fit a second trend, or apply the decision/blend weight.
/// The research owner must establish outcome availability/provenance at C;
/// sealed cohort prefixes alone are not historical online-availability proof.
pub fn probability_forecast(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
) -> Result<ProbabilityForecast, RenderError> {
    if !matches!(recipe, ForecastRecipe::DecayedProbability { .. }) {
        return Err(RenderError::InvalidForecast(ForecastError::WrongMethod));
    }
    validate_model_source(artifact, recipe)?;
    artifact
        .forecast
        .as_ref()
        .and_then(|model| model.probability.as_ref())
        .map(|model| model.estimate)
        .ok_or(RenderError::NoFiniteDays)
}

/// One per-setup render, never a shared final study image. The exact saved
/// scalar is checked before allocating a pixmap. All points, SVG, pixels and
/// encoded scratch belong to this call and are released on every return path.
pub fn render_png_with_probability(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
    raw_signal_at: i64,
    saved_forecast_r: f64,
) -> Result<Vec<u8>, RenderError> {
    let svg = render_svg_with_probability(artifact, recipe, raw_signal_at, saved_forecast_r)?;
    rasterize_svg(&svg)
}

/// Explicit v3 only, using the method-specific curve already sealed by core.
/// This never upgrades a legacy artifact or substitutes a presentation fit.
pub fn render_png_with_model(
    artifact: &ExpectancyArtifact,
    raw_signal_at: i64,
    saved_forecast_r: f64,
) -> Result<Vec<u8>, RenderError> {
    let recipe = artifact
        .source
        .forecast
        .ok_or(RenderError::ForecastMismatch)?;
    rasterize_svg(&render_svg_with_model(
        artifact,
        recipe,
        raw_signal_at,
        saved_forecast_r,
    )?)
}

pub fn render_png_with_objective(
    artifact: &ExpectancyArtifact,
    objective: ExpectancyObjective,
    raw_signal_at: i64,
    saved_decision_r: f64,
) -> Result<Vec<u8>, RenderError> {
    rasterize_svg(&render_svg_with_objective(
        artifact,
        objective,
        raw_signal_at,
        saved_decision_r,
    )?)
}

fn render_svg_with_objective(
    artifact: &ExpectancyArtifact,
    objective: ExpectancyObjective,
    raw_signal_at: i64,
    saved_decision_r: f64,
) -> Result<String, RenderError> {
    let series = artifact
        .objective_series(objective)
        .map_err(RenderError::InvalidArtifact)?;
    let decision = series
        .decision_r
        .as_ref()
        .copied()
        .ok_or_else(|| match series.decision_r {
            setups_domain::expectancy::MetricAvailability::Unavailable { reason } => {
                RenderError::SeriesUnavailable(reason)
            }
            _ => RenderError::ForecastMismatch,
        })?;
    let mean_r = series
        .mean_r
        .as_ref()
        .copied()
        .ok_or(RenderError::SeriesUnavailable(
            ExpectancyUnavailableReason::MissingSelectedMean,
        ))?;
    let points = daily_points(artifact)?;
    let (tp_count, sl_count) = points.iter().try_fold((0u64, 0u64), |(tp, sl), p| {
        Ok::<_, RenderError>((
            tp.checked_add(p.tp_count)
                .ok_or(RenderError::InvalidGeometry)?,
            sl.checked_add(p.sl_count)
                .ok_or(RenderError::InvalidGeometry)?,
        ))
    })?;
    let model = PlotForecast {
        mean_r,
        forecast_r: decision,
        tp_count,
        sl_count,
        method: series.method,
        curve: series
            .curve
            .into_iter()
            .map(|(at, value)| ((at - artifact.origin_unix as f64) / 86400.0, value))
            .collect(),
    };
    let method = if matches!(
        objective.forecast,
        Some(ForecastRecipe::DecayedProbability { .. })
    ) {
        "вероятностный"
    } else {
        "по тренду"
    };
    let label = match objective.trend_mode {
        TrendMode::Mean => "Среднее M".to_string(),
        TrendMode::Forecast => format!("Прогноз F · {method}"),
        TrendMode::Blend => format!(
            "Смесь B · {}/{} · {method}",
            100 - objective.trend_blend_lambda.percent(),
            objective.trend_blend_lambda.percent()
        ),
    };
    render_svg_projection(
        artifact,
        raw_signal_at,
        saved_decision_r,
        model,
        OBJECTIVE_RENDER_VERSION,
        &format!("{label} · ретроспективная оценка"),
    )
}

fn validate_model_source(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
) -> Result<(), RenderError> {
    artifact.validate().map_err(RenderError::InvalidArtifact)?;
    if artifact.calculation_version
        != setups_domain::expectancy::EXPECTANCY_MODEL_CALCULATION_VERSION
        || artifact.source.forecast != Some(recipe)
    {
        return Err(RenderError::ForecastMismatch);
    }
    Ok(())
}

pub struct StudySeries {
    pub mean_r: f64,
    pub trend_method: Option<&'static str>,
    pub trend_points: Vec<(f64, f64)>,
}

pub fn study_series(
    artifact: &ExpectancyArtifact,
    study: StudyTrend,
) -> Result<StudySeries, RenderError> {
    artifact.validate().map_err(RenderError::InvalidArtifact)?;
    let points = daily_points(artifact)?;
    Geometry::from_points(points, None)?;
    let mean = period_mean_r(artifact, points);
    let trend = display_trend(points, mean, study)?;
    Ok(StudySeries {
        mean_r: mean,
        trend_method: trend.as_ref().map(|trend| trend.method),
        trend_points: trend.map(|trend| trend.points).unwrap_or_default(),
    })
}

#[cfg(test)]
fn attr_after(svg: &str, marker: &str) -> f64 {
    let start = svg.find(marker).expect(marker) + marker.len();
    let end = svg[start..].find('"').expect("closing quote") + start;
    svg[start..end].parse().expect("numeric attribute")
}

#[cfg(test)]
fn probability_request(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
) -> Result<ProbabilityCurveRequest, RenderError> {
    artifact.validate().map_err(RenderError::InvalidArtifact)?;
    let points = daily_points(artifact)?;
    Ok(ProbabilityCurveRequest {
        recipe,
        q: artifact.source.tp_pct / artifact.source.sl_pct,
        observation_cutoff: artifact.source.provenance.observation_cutoff,
        daily: points
            .iter()
            .map(|point| ProbabilityDay {
                day_start_unix: point.day_start_unix,
                tp_count: point.tp_count,
                sl_count: point.sl_count,
            })
            .collect(),
    })
}

fn daily_points(artifact: &ExpectancyArtifact) -> Result<&[UtcDailyPoint], RenderError> {
    match &artifact.daily {
        setups_domain::expectancy::MetricAvailability::Available { value } => Ok(value),
        setups_domain::expectancy::MetricAvailability::Unavailable { reason } => {
            Err(RenderError::SeriesUnavailable(*reason))
        }
    }
}

fn rasterize_svg(svg: &str) -> Result<Vec<u8>, RenderError> {
    if svg.len() > MAX_SVG_BYTES {
        return Err(RenderError::SvgTooLarge);
    }
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
    let tree = usvg::Tree::from_str(svg, &options).map_err(|_| RenderError::SvgInvalid)?;
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
        trend: Option<&DisplayTrend>,
    ) -> Result<Self, RenderError> {
        let mut minimum = 0.0_f64;
        let mut maximum = 1.0_f64;
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
        // Include the displayed curve (including spline overshoot), but never
        // uncertainty bands or decorative padding in the labelled bounds.
        if let Some(trend) = trend {
            for &(_, value) in &trend.points {
                minimum = minimum.min(value);
                maximum = maximum.max(value);
            }
        }
        let (y_min, y_max) = (minimum, maximum);
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

fn write_grid(svg: &mut BoundedSvg, geometry: Geometry) -> fmt::Result {
    let last_x = geometry.first_x + geometry.x_span;
    let day_count = (last_x - geometry.first_x).max(0.0);
    let day_step = if day_count <= 250.0 {
        1.0
    } else {
        (day_count / 250.0).ceil()
    };
    let mut day = geometry.first_x.ceil();
    while day < last_x {
        let x = geometry.x(day);
        write!(
            svg,
            r#"<line class="grid-day" x1="{x:.3}" y1="{TOP}" x2="{x:.3}" y2="{BOTTOM}" stroke="{GRID}" stroke-width="1"/>"#
        )?;
        day += day_step;
    }
    if geometry.y_max - geometry.y_min <= 200.0 {
        let mut level = geometry.y_min.ceil() as i64;
        while (level as f64) <= geometry.y_max {
            let value = level as f64;
            if value != 0.0 && value != geometry.y_min && value != geometry.y_max {
                let y = geometry.y(value);
                write!(
                    svg,
                    r#"<line class="grid-level" x1="{LEFT}" y1="{y:.3}" x2="{RIGHT}" y2="{y:.3}" stroke="{GRID}" stroke-width="1"/>"#
                )?;
            }
            level += 1;
        }
    }
    Ok(())
}

/// <=257 sampled points owned by one render and released with its SVG. No
/// confidence interval is retained or drawn, for straight or curved fits.
#[derive(Debug, PartialEq)]
struct DisplayTrend {
    method: &'static str,
    points: Vec<(f64, f64)>,
}

fn display_trend(
    points: &[UtcDailyPoint],
    mean: f64,
    study: StudyTrend,
) -> Result<Option<DisplayTrend>, RenderError> {
    let mut finite = points.iter().filter(|point| point.mean_r.is_some());
    let Some(first) = finite.next() else {
        return Ok(None);
    };
    let Some(last) = finite.next_back() else {
        return Ok(None);
    };
    if study.trend_mode == TrendMode::Mean {
        return Ok(Some(DisplayTrend {
            method: "mean",
            points: vec![(first.x_utc_days, mean), (last.x_utc_days, mean)],
        }));
    }
    let input: Vec<_> = points
        .iter()
        .map(|point| {
            let weight = u32::try_from(point.row_count)
                .map_err(|_| RenderError::InvalidTrend(TrendError::InvalidDayOrValue))?;
            Ok(TrendInput {
                day: point.x_utc_days,
                value: point.mean_r,
                weight,
            })
        })
        .collect::<Result<_, RenderError>>()?;
    let Some(fit) =
        fit_weighted_trend(&input, study.trend_bend_days).map_err(RenderError::InvalidTrend)?
    else {
        return Ok(None);
    };
    let summary = fit.summary();
    Ok(Some(DisplayTrend {
        method: summary.method,
        points: summary
            .curve
            .iter()
            .map(|point| {
                let value = if study.trend_mode == TrendMode::Blend {
                    study.trend_blend_lambda.combine(mean, point.value)
                } else {
                    point.value
                };
                (point.day, value)
            })
            .collect(),
    }))
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

fn period_win_rate_pct(points: &[UtcDailyPoint]) -> Option<f64> {
    // Domain validation bounds total TP and SL individually to u64; u128 also
    // safely adds both totals. Unclosed/null rows never enter this denominator.
    let (tp, sl) = points.iter().fold((0_u128, 0_u128), |(tp, sl), point| {
        (
            tp + u128::from(point.tp_count),
            sl + u128::from(point.sl_count),
        )
    });
    (tp + sl > 0).then(|| 100.0 * tp as f64 / (tp + sl) as f64)
}

#[cfg(test)]
fn render_svg(artifact: &ExpectancyArtifact) -> Result<String, RenderError> {
    render_svg_with_trend(artifact, StudyTrend::default())
}

fn render_svg_with_trend(
    artifact: &ExpectancyArtifact,
    study: StudyTrend,
) -> Result<String, RenderError> {
    artifact.validate().map_err(RenderError::InvalidArtifact)?;
    let points = artifact.daily.as_ref().ok_or_else(|| {
        let setups_domain::expectancy::MetricAvailability::Unavailable { reason } = &artifact.daily
        else {
            unreachable!("available handled above");
        };
        RenderError::SeriesUnavailable(*reason)
    })?;
    // Geometry rejects all-null/empty series before presentation aggregates.
    Geometry::from_points(points, None)?;
    let mean = period_mean_r(artifact, points);
    let trend = display_trend(points, mean, study)?;
    let geometry = Geometry::from_points(points, trend.as_ref())?;
    let title_color = if mean > 0.0 {
        POSITIVE
    } else if mean < 0.0 {
        NEGATIVE
    } else {
        FOREGROUND
    };
    let mean_label = risk_label(mean);
    let win_rate = period_win_rate_pct(points)
        .map_or_else(|| "недоступно".into(), |rate| format!("{rate:.1}%"));
    let mut svg = BoundedSvg(String::with_capacity(
        (points.len() * 128 + 8192).min(MAX_SVG_BYTES),
    ));
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="900" height="320" viewBox="0 0 900 320" font-family="{FONT_FAMILY}"><rect width="900" height="320" fill="{BACKGROUND}"/><text id="title" x="450" y="28" text-anchor="middle" font-size="14" fill="{FOREGROUND}">{TITLE} · Среднее МО: <tspan fill="{title_color}">{mean_label}</tspan> · Win rate: {win_rate}</text>"#
    )?;
    write_axes(&mut svg, geometry)?;
    write_grid(&mut svg, geometry)?;
    write_daily_segments(&mut svg, points, geometry)?;
    if let Some(trend) = &trend {
        write!(
            svg,
            r#"<polyline id="trend" data-method="{}" fill="none" stroke="{TREND}" stroke-width="2" stroke-dasharray="4 4" stroke-linejoin="round" points=""#,
            trend.method,
        )?;
        for &(day, value) in &trend.points {
            write!(svg, "{:.3},{:.3} ", geometry.x(day), geometry.y(value))?;
        }
        write!(svg, "\"/>")?;
    }
    write_daily_points(&mut svg, points, geometry)?;
    if let Some(trend) = &trend {
        let &(first_x, start_r) = trend.points.first().expect("two finite dates");
        let &(last_x, end_r) = trend.points.last().expect("two finite dates");
        let close = geometry.x(last_x) - geometry.x(first_x) < 2.0 * LABEL_WIDTH;
        trend_label(
            &mut svg,
            "trend-start",
            geometry.x(first_x).clamp(LEFT, RIGHT - LABEL_WIDTH),
            (geometry.y(start_r) - 12.0).clamp(48.0, BOTTOM - 6.0),
            "start",
            start_r,
        )?;
        trend_label(
            &mut svg,
            "trend-end",
            geometry.x(last_x).clamp(LEFT + LABEL_WIDTH, RIGHT),
            (geometry.y(end_r) + if close { 20.0 } else { -12.0 }).clamp(48.0, BOTTOM - 6.0),
            "end",
            end_r,
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

fn write_axes(svg: &mut BoundedSvg, geometry: Geometry) -> Result<(), RenderError> {
    write!(
        svg,
        r#"<g fill="none" stroke="{AXIS}" stroke-width="1" stroke-opacity="0.4"><path d="M {LEFT} {TOP} V {BOTTOM} H {RIGHT}"/></g><text x="42" y="56" text-anchor="middle" font-size="12" fill="{AXIS}">R</text>"#
    )?;
    let zero = geometry.y(0.0);
    for (id, value) in [
        ("axis-min", geometry.y_min),
        ("axis-mid", geometry.y_min / 2.0 + geometry.y_max / 2.0),
        ("axis-max", geometry.y_max),
    ] {
        let y = geometry.y(value);
        if value == 0.0 || (id == "axis-mid" && (y - zero).abs() < 14.0) {
            continue;
        }
        write!(
            svg,
            r#"<line x1="{LEFT}" y1="{y:.3}" x2="{RIGHT}" y2="{y:.3}" stroke="{AXIS}" stroke-opacity="0.18"/><text id="{id}" x="70" y="{:.3}" text-anchor="end" font-size="11" fill="{AXIS}">{}</text>"#,
            y + 4.0,
            risk_label(value)
        )?;
    }
    write!(
        svg,
        r#"<line id="zero" x1="{LEFT}" y1="{zero:.3}" x2="{RIGHT}" y2="{zero:.3}" stroke="{AXIS}" stroke-opacity="0.55"/><text id="zero-label" x="70" y="{:.3}" text-anchor="end" font-size="11" fill="{AXIS}">0</text>"#,
        zero + if zero - TOP < 14.0 { 18.0 } else { 4.0 }
    )?;
    Ok(())
}

fn write_daily_segments(
    svg: &mut BoundedSvg,
    points: &[UtcDailyPoint],
    geometry: Geometry,
) -> Result<(), RenderError> {
    // Null buckets break the line; absent calendar dates retain their distance.
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
    Ok(())
}

fn write_daily_points(
    svg: &mut BoundedSvg,
    points: &[UtcDailyPoint],
    geometry: Geometry,
) -> Result<(), RenderError> {
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
    Ok(())
}

fn utc_day_offset(origin: i64, at: i64) -> Result<f64, RenderError> {
    let seconds = at.checked_sub(origin).ok_or(RenderError::InvalidGeometry)?;
    Ok(seconds as f64 / setups_domain::expectancy::UTC_DAY_SECONDS as f64)
}

struct PlotForecast {
    mean_r: f64,
    forecast_r: f64,
    tp_count: u64,
    sl_count: u64,
    method: &'static str,
    // Display coordinates only; numeric values/curve samples come from core.
    curve: Vec<(f64, Option<f64>)>,
}

fn plot_forecast(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
) -> Result<PlotForecast, RenderError> {
    validate_model_source(artifact, recipe)?;
    let points = daily_points(artifact)?;
    let model = artifact
        .forecast
        .as_ref()
        .ok_or(RenderError::NoFiniteDays)?;
    match recipe {
        ForecastRecipe::DecayedProbability { .. } => {
            let probability = model
                .probability
                .as_ref()
                .ok_or(RenderError::ForecastMismatch)?;
            let estimate = match probability.estimate {
                ProbabilityForecast::Available { value } => value,
                ProbabilityForecast::Unavailable { reason } => {
                    return Err(RenderError::ForecastUnavailable(reason));
                }
            };
            Ok(PlotForecast {
                mean_r: estimate.mean_r,
                forecast_r: estimate.forecast_r,
                tp_count: estimate.tp_count,
                sl_count: estimate.sl_count,
                method: "decayed_probability",
                curve: probability
                    .curve
                    .iter()
                    .map(|point| {
                        Ok((
                            utc_day_offset(artifact.origin_unix, point.at_unix)?,
                            point.forecast_r,
                        ))
                    })
                    .collect::<Result<_, RenderError>>()?,
            })
        }
        ForecastRecipe::Trend { .. } => {
            let trend = model.trend.as_ref().ok_or(RenderError::NoFiniteDays)?;
            let (tp_count, sl_count) =
                points.iter().try_fold((0u64, 0u64), |(tp, sl), point| {
                    Ok::<_, RenderError>((
                        tp.checked_add(point.tp_count)
                            .ok_or(RenderError::InvalidGeometry)?,
                        sl.checked_add(point.sl_count)
                            .ok_or(RenderError::InvalidGeometry)?,
                    ))
                })?;
            let mut curve: Vec<_> = trend
                .curve
                .iter()
                .map(|point| {
                    (
                        point.day - artifact.origin_unix as f64 / 86400.0,
                        Some(point.value),
                    )
                })
                .collect();
            // TrendSummary is fitted only over observed cohorts. Core freezes
            // its endpoint at C; extending to C is NOT slope extrapolation.
            curve.push((
                utc_day_offset(
                    artifact.origin_unix,
                    artifact.source.provenance.observation_cutoff,
                )?,
                Some(trend.end.value),
            ));
            Ok(PlotForecast {
                mean_r: period_mean_r(artifact, points),
                forecast_r: trend.end.value,
                tp_count,
                sl_count,
                method: trend.method,
                curve,
            })
        }
    }
}

fn probability_geometry(
    points: &[UtcDailyPoint],
    model: &PlotForecast,
    origin: i64,
    raw_signal_at: i64,
) -> Result<Geometry, RenderError> {
    let mut geometry = Geometry::from_points(points, None)?;
    // The same scale includes actual daily R, M, every model point and the
    // raw-close marker, not just fitted endpoints or a decorative spline.
    for value in model
        .curve
        .iter()
        .filter_map(|point| point.1)
        .chain([model.mean_r, model.forecast_r])
    {
        if !value.is_finite() {
            return Err(RenderError::InvalidGeometry);
        }
        geometry.y_min = geometry.y_min.min(value);
        geometry.y_max = geometry.y_max.max(value);
    }
    geometry.x_span = utc_day_offset(origin, raw_signal_at)? - geometry.first_x;
    if !geometry.x_span.is_finite()
        || geometry.x_span < 0.0
        || !(geometry.y_max - geometry.y_min).is_finite()
    {
        return Err(RenderError::InvalidGeometry);
    }
    Ok(geometry)
}

fn render_svg_with_probability(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
    raw_signal_at: i64,
    saved_forecast_r: f64,
) -> Result<String, RenderError> {
    if !matches!(recipe, ForecastRecipe::DecayedProbability { .. }) {
        return Err(RenderError::InvalidForecast(ForecastError::WrongMethod));
    }
    render_svg_with_model(artifact, recipe, raw_signal_at, saved_forecast_r)
}

fn render_svg_with_model(
    artifact: &ExpectancyArtifact,
    recipe: ForecastRecipe,
    raw_signal_at: i64,
    saved_forecast_r: f64,
) -> Result<String, RenderError> {
    let model = plot_forecast(artifact, recipe)?;
    render_svg_projection(
        artifact,
        raw_signal_at,
        saved_forecast_r,
        model,
        PROBABILITY_RENDER_VERSION,
        "Ретроспективная модельная оценка",
    )
}

fn render_svg_projection(
    artifact: &ExpectancyArtifact,
    raw_signal_at: i64,
    saved_forecast_r: f64,
    model: PlotForecast,
    render_version: u32,
    series_label: &str,
) -> Result<String, RenderError> {
    let cutoff = artifact.source.provenance.observation_cutoff;
    if raw_signal_at < cutoff {
        return Err(RenderError::InvalidForecast(ForecastError::BeforeCutoff));
    }
    if !saved_forecast_r.is_finite() || model.forecast_r != saved_forecast_r {
        return Err(RenderError::ForecastMismatch);
    }
    // Matches the caption's signed, two-decimal formatting, including -0.00.
    // An unreasonably wide label degrades to text, never clips a false value.
    let points = daily_points(artifact)?;
    let geometry = probability_geometry(points, &model, artifact.origin_unix, raw_signal_at)?;
    let cutoff_x = geometry.x(utc_day_offset(artifact.origin_unix, cutoff)?);
    let marker_x = geometry.x(utc_day_offset(artifact.origin_unix, raw_signal_at)?);
    let marker_y = geometry.y(saved_forecast_r);
    let title_color = if model.mean_r > 0.0 {
        POSITIVE
    } else if model.mean_r < 0.0 {
        NEGATIVE
    } else {
        FOREGROUND
    };
    let win_rate = period_win_rate_pct(points).ok_or(RenderError::NoFiniteDays)?;
    let mut svg = BoundedSvg(String::with_capacity(
        (points.len() * 160 + 8192).min(MAX_SVG_BYTES),
    ));
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="900" height="320" viewBox="0 0 900 320" font-family="{FONT_FAMILY}" data-render-version="{render_version}"><rect width="900" height="320" fill="{BACKGROUND}"/><text id="title" x="450" y="28" text-anchor="middle" font-size="14" fill="{FOREGROUND}">{TITLE} · Среднее МО: <tspan fill="{title_color}">{}</tspan> · Win rate: {win_rate:.1}%</text><text id="retrospective-label" x="80" y="49" font-size="11" fill="{TREND}">{series_label}</text>"#,
        risk_label(model.mean_r)
    )?;
    write_axes(&mut svg, geometry)?;
    write_grid(&mut svg, geometry)?;
    write_daily_segments(&mut svg, points, geometry)?;
    // Retrospective prefixes come only from the shared evaluator. Null prefixes
    // are not zeroes and break segments; no timestamp/label-availability claim
    // is inferred from the raw-signal cohort date.
    let mut segment_start = 0;
    for end in 0..=model.curve.len() {
        if end == model.curve.len() || model.curve[end].1.is_none() {
            if end > segment_start {
                write!(
                    svg,
                    r#"<polyline class="probability-model" data-method="{}" fill="none" stroke="{TREND}" stroke-width="2" points=""#,
                    model.method
                )?;
                for point in &model.curve[segment_start..end] {
                    write!(
                        svg,
                        "{:.3},{:.3} ",
                        geometry.x(point.0),
                        geometry.y(point.1.expect("finite model segment"))
                    )?;
                }
                write!(svg, "\"/>")?;
            }
            segment_start = end + 1;
        }
    }
    write!(
        svg,
        r#"<line id="observation-cutoff" data-at-unix="{}" x1="{cutoff_x:.3}" y1="{TOP}" x2="{cutoff_x:.3}" y2="{BOTTOM}" stroke="{AXIS}" stroke-dasharray="3 4"/><text id="cutoff-label" x="{:.3}" y="62" text-anchor="end" font-size="11" fill="{AXIS}">C</text><line id="frozen-forecast" data-value="{saved_forecast_r}" x1="{cutoff_x:.3}" y1="{marker_y:.3}" x2="{marker_x:.3}" y2="{marker_y:.3}" stroke="{TREND}" stroke-width="2" stroke-dasharray="4 4"/>"#,
        cutoff,
        cutoff_x.clamp(LEFT + 12.0, RIGHT)
    )?;
    write_daily_points(&mut svg, points, geometry)?;
    write!(
        svg,
        r#"<circle id="raw-close-marker" data-at-unix="{raw_signal_at}" data-forecast-r="{saved_forecast_r}" cx="{marker_x:.3}" cy="{marker_y:.3}" r="5" fill="{TREND}" stroke="{BACKGROUND}" stroke-width="2"/>"#
    )?;
    let label_at_left = marker_x > RIGHT - LABEL_WIDTH;
    let label_x = if label_at_left { marker_x - 10.0 } else { marker_x + 10.0 };
    write!(
        svg,
        r#"<text id="raw-close-label" data-forecast-r="{saved_forecast_r}" x="{label_x:.3}" y="{:.3}" text-anchor="{}" font-size="11" fill="{TREND}">{}</text>"#,
        (marker_y + 4.0).clamp(TOP, BOTTOM),
        if label_at_left { "end" } else { "start" },
        risk_label(saved_forecast_r)
    )?;
    // Reserve the right edge for the explicit C/raw-close annotation. Calendar
    // labels are spaced in pixels; a very late raw-close never overlaps them.
    let mut previous_x: Option<f64> = None;
    let step = points.len().div_ceil(8).max(1);
    for point in points.iter().step_by(step) {
        let x = geometry.x(point.x_utc_days);
        if x > RIGHT - 70.0 || previous_x.is_some_and(|previous| x - previous < 70.0) {
            continue;
        }
        write!(
            svg,
            r#"<text class="date" x="{x:.3}" y="283" text-anchor="middle" font-size="11" fill="{AXIS}">{}.{}</text>"#,
            &point.date[8..10],
            &point.date[5..7]
        )?;
        previous_x = Some(x);
    }
    write!(
        svg,
        r#"<text x="476" y="308" text-anchor="middle" font-size="11" fill="{AXIS}">Дата сетапа (UTC) · TP: {} · SL: {} · Закрыто: {}</text></svg>"#,
        model.tp_count,
        model.sl_count,
        model
            .tp_count
            .checked_add(model.sl_count)
            .ok_or(RenderError::InvalidGeometry)?
    )?;
    Ok(svg.0)
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

    fn curved_fixture() -> ExpectancyArtifact {
        let mut input = input();
        input.selected_cell = None;
        input.source.tp_pct = 4.0;
        input.source.provenance.to_unix = ORIGIN + 43 * 86400;
        input.source.provenance.observation_cutoff = input.source.provenance.to_unix;
        input.daily = MetricAvailability::Available {
            value: (0..43)
                .map(|day| {
                    let tp = if day < 14 {
                        1 + day / 2
                    } else if day < 28 {
                        8 - (day - 14) / 2
                    } else {
                        1 + (day - 28) / 2
                    };
                    let (tp_count, sl_count) = if day == 10 || day == 28 {
                        (0, 0)
                    } else {
                        (tp as u64, (10 - tp) as u64)
                    };
                    UtcDailyInput {
                        day_start_unix: ORIGIN + day * 86400,
                        row_count: 10 + day as u64 % 4,
                        tp_count,
                        sl_count,
                        mean_r: (tp_count + sl_count > 0)
                            .then(|| (tp_count as f64 * 4.0 - sl_count as f64) / 10.0),
                    }
                })
                .collect(),
        };
        build_expectancy_artifact(input).unwrap()
    }

    fn explicit_fixture(
        mut artifact: ExpectancyArtifact,
        recipe: ForecastRecipe,
    ) -> ExpectancyArtifact {
        artifact.source.forecast = Some(recipe);
        let cell = artifact.selected_cell.as_ref();
        let daily = match artifact.daily {
            MetricAvailability::Available { value } => MetricAvailability::Available {
                value: value
                    .into_iter()
                    .map(|p| UtcDailyInput {
                        day_start_unix: p.day_start_unix,
                        row_count: p.row_count,
                        tp_count: p.tp_count,
                        sl_count: p.sl_count,
                        mean_r: p.mean_r,
                    })
                    .collect(),
            },
            MetricAvailability::Unavailable { reason } => {
                MetricAvailability::Unavailable { reason }
            }
        };
        build_expectancy_artifact(ExpectancyInput {
            source: artifact.source,
            selected_cell: cell.map(|cell| setups_domain::expectancy::SelectedCellInput {
                raw_r: cell.raw_r.as_ref().copied(),
                tp_count: cell.tp_count,
                sl_count: cell.sl_count,
            }),
            matrix_mean_r: artifact.matrix_mean_r.as_ref().copied(),
            daily,
        })
        .unwrap()
    }

    #[test]
    fn objective_png_uses_the_entire_matching_core_series_and_exact_scalar_in_all_modes() {
        for recipe in [
            None,
            Some(ForecastRecipe::engineering_baseline()),
            Some(ForecastRecipe::Trend {
                version: 1,
                bend_days: TrendBendDays::Seven,
            }),
        ] {
            let artifact =
                recipe.map_or_else(fixture, |recipe| explicit_fixture(fixture(), recipe));
            let sealed = artifact.to_json().unwrap();
            for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
                let objective = ExpectancyObjective {
                    version: 1,
                    trend_mode: mode,
                    trend_bend_days: match recipe {
                        Some(ForecastRecipe::Trend { bend_days, .. }) => bend_days,
                        _ => TrendBendDays::Linear,
                    },
                    trend_blend_lambda: TrendBlendLambda::default(),
                    forecast: recipe,
                };
                let series = artifact.objective_series(objective).unwrap();
                let value = *series.decision_r.as_ref().unwrap();
                let raw = artifact.source.provenance.observation_cutoff + 86400;
                let svg = render_svg_with_objective(&artifact, objective, raw, value).unwrap();
                assert!(svg.contains("data-render-version=\"3\""));
                assert!(svg.contains(&format!("data-forecast-r=\"{value}\"")));
                assert!(svg.contains("Динамика матожидания алгоритма"));
                let plot = PlotForecast {
                    mean_r: *series.mean_r.as_ref().unwrap(),
                    forecast_r: value,
                    tp_count: 0,
                    sl_count: 0,
                    method: series.method,
                    curve: series
                        .curve
                        .iter()
                        .map(|&(at, v)| ((at - artifact.origin_unix as f64) / 86400.0, v))
                        .collect(),
                };
                let geometry = probability_geometry(
                    daily_points(&artifact).unwrap(),
                    &plot,
                    artifact.origin_unix,
                    raw,
                )
                .unwrap();
                for &(at, value) in &series.curve {
                    if let Some(value) = value {
                        assert!(
                            svg.contains(&format!(
                                "{:.3},{:.3} ",
                                geometry.x((at - artifact.origin_unix as f64) / 86400.0),
                                geometry.y(value)
                            )),
                            "entire objective curve, not only endpoint"
                        );
                    }
                }
                assert_eq!(
                    render_png_with_objective(&artifact, objective, raw, value + 0.01),
                    Err(RenderError::ForecastMismatch)
                );
                let png = render_png_with_objective(&artifact, objective, raw, value).unwrap();
                assert!(png.len() < MAX_PNG_BYTES);
                assert_eq!(
                    png,
                    render_png_with_objective(&artifact, objective, raw, value).unwrap()
                );
                assert_eq!(artifact.to_json().unwrap(), sealed);
            }
        }
    }

    #[test]
    fn probability_render_uses_canonical_pure_forecast_and_frozen_raw_close_not_blend() {
        let artifact = explicit_fixture(fixture(), ForecastRecipe::engineering_baseline());
        let sealed = artifact.to_json().unwrap();
        let recipe = ForecastRecipe::engineering_baseline();
        let scalar = probability_forecast(&artifact, recipe).unwrap();
        let estimate = *scalar.as_ref().unwrap();
        let request = probability_request(&artifact, recipe).unwrap();
        assert_eq!(
            scalar,
            evaluate_probability(
                recipe,
                request.q,
                request.observation_cutoff,
                &request.daily
            )
            .unwrap()
        );
        let core = probability_curve(&request).unwrap();
        assert_eq!(
            core.curve.last().unwrap().forecast_r,
            Some(estimate.forecast_r)
        );
        let blend = setups_domain::forecast::decision_r(
            TrendMode::Blend,
            Some(estimate.mean_r),
            Some(estimate.forecast_r),
            TrendBlendLambda::default(),
        )
        .unwrap()
        .unwrap();
        assert_ne!(blend, estimate.forecast_r);
        let cutoff = request.observation_cutoff;
        let svg =
            render_svg_with_probability(&artifact, recipe, cutoff + 3 * 86400, estimate.forecast_r)
                .unwrap();
        assert!(svg.contains("Ретроспективная модельная оценка"));
        assert!(svg.contains(&format!("data-forecast-r=\"{}\"", estimate.forecast_r)));
        assert!(svg.contains(&format!("data-at-unix=\"{}\"", cutoff + 3 * 86400)));
        assert!(svg.contains("class=\"grid-level\""));
        assert!(
            !svg.contains("id=\"trend\"") && !svg.contains("p_spline") && !svg.contains("<polygon")
        );
        let geometry = probability_geometry(
            artifact.daily.as_ref().unwrap(),
            &plot_forecast(&artifact, recipe).unwrap(),
            artifact.origin_unix,
            cutoff + 3 * 86400,
        )
        .unwrap();
        let forecast_y = geometry.y(estimate.forecast_r);
        assert!(svg.contains(&format!("id=\"frozen-forecast\" data-value=\"{}\" x1=\"{:.3}\" y1=\"{forecast_y:.3}\" x2=\"872.000\" y2=\"{forecast_y:.3}\"", estimate.forecast_r, geometry.x(11.0))));
        for raw in [cutoff, cutoff + 86400, cutoff + 30 * 86400] {
            assert_eq!(scalar.at(raw).unwrap(), Some(estimate.forecast_r));
            let bytes =
                render_png_with_probability(&artifact, recipe, raw, estimate.forecast_r).unwrap();
            setups_relay_protocol::v2::validate_png(&bytes).unwrap();
            assert_eq!(
                bytes,
                render_png_with_probability(&artifact, recipe, raw, estimate.forecast_r).unwrap()
            );
        }
        assert_ne!(
            render_png_with_probability(&artifact, recipe, cutoff, estimate.forecast_r).unwrap(),
            render_png_with_probability(
                &artifact,
                recipe,
                cutoff + 30 * 86400,
                estimate.forecast_r
            )
            .unwrap()
        );
        assert_eq!(
            render_png_with_probability(&artifact, recipe, cutoff, blend),
            Err(RenderError::ForecastMismatch)
        );
        assert_eq!(
            render_png_with_probability(&artifact, recipe, cutoff - 1, estimate.forecast_r),
            Err(RenderError::InvalidForecast(ForecastError::BeforeCutoff))
        );
        assert_eq!(artifact.to_json().unwrap(), sealed);
        assert_eq!(RENDER_VERSION, 1);
        assert_eq!(PROBABILITY_RENDER_VERSION, 2);
    }

    #[test]
    fn probability_counts_ignore_unclosed_rows_and_never_invent_prior_only_forecast() {
        let artifact = explicit_fixture(fixture(), ForecastRecipe::engineering_baseline());
        let recipe = ForecastRecipe::engineering_baseline();
        let scalar = probability_forecast(&artifact, recipe).unwrap();
        let estimate = *scalar.as_ref().unwrap();
        assert_eq!((estimate.tp_count, estimate.sl_count), (3, 2));
        let mut extra_rows = artifact.clone();
        let MetricAvailability::Available { value: points } = &mut extra_rows.daily else {
            unreachable!()
        };
        for point in points {
            point.row_count += 100_000_000;
        }
        assert_eq!(probability_forecast(&extra_rows, recipe).unwrap(), scalar);
        let cutoff = artifact.source.provenance.observation_cutoff;
        assert_eq!(
            render_png_with_probability(&artifact, recipe, cutoff, estimate.forecast_r).unwrap(),
            render_png_with_probability(&extra_rows, recipe, cutoff, estimate.forecast_r).unwrap()
        );
        let single = explicit_fixture(with_outcomes(2.0, 1.0, &[(0, 1, 0)]), recipe);
        let single_r = probability_forecast(&single, recipe)
            .unwrap()
            .as_ref()
            .unwrap()
            .forecast_r;
        setups_relay_protocol::v2::validate_png(
            &render_png_with_probability(&single, recipe, cutoff, single_r).unwrap(),
        )
        .unwrap();
        for empty in [
            with_outcomes(2.0, 1.0, &[]),
            with_outcomes(2.0, 1.0, &[(0, 0, 0)]),
        ] {
            let empty = explicit_fixture(empty, recipe);
            assert_eq!(
                probability_forecast(&empty, recipe).unwrap(),
                ProbabilityForecast::Unavailable {
                    reason: ForecastUnavailableReason::NoClosedOutcomes
                }
            );
            assert_eq!(
                render_png_with_probability(&empty, recipe, cutoff, 0.0),
                Err(RenderError::ForecastUnavailable(
                    ForecastUnavailableReason::NoClosedOutcomes
                ))
            );
        }
        assert_eq!(
            probability_forecast(
                &artifact,
                ForecastRecipe::Trend {
                    version: 1,
                    bend_days: TrendBendDays::Linear
                }
            ),
            Err(RenderError::InvalidForecast(ForecastError::WrongMethod))
        );
        assert_eq!(
            render_png_with_probability(&artifact, recipe, cutoff, f64::NAN),
            Err(RenderError::ForecastMismatch)
        );
        assert_eq!(
            rasterize_svg(&"x".repeat(MAX_SVG_BYTES + 1)),
            Err(RenderError::SvgTooLarge)
        );
        assert_eq!(
            probability_forecast(&fixture(), recipe),
            Err(RenderError::ForecastMismatch)
        );
        assert_eq!(
            render_png_with_model(&fixture(), cutoff, estimate.forecast_r),
            Err(RenderError::ForecastMismatch)
        );
    }

    #[test]
    fn explicit_trend_uses_sealed_core_summary_and_freezes_endpoint_at_actual_cutoff() {
        let mut old = curved_fixture();
        old.source.provenance.observation_cutoff = old.to_unix + 5 * 86400;
        let recipe = ForecastRecipe::Trend {
            version: 1,
            bend_days: TrendBendDays::Seven,
        };
        let artifact = explicit_fixture(old, recipe);
        let cutoff = artifact.source.provenance.observation_cutoff;
        let trend = artifact.forecast.as_ref().unwrap().trend.as_ref().unwrap();
        assert!(artifact.trend.is_none(), "new model never uses legacy OLS");
        assert_eq!(
            artifact
                .forecast_at_raw_signal(cutoff + 86400)
                .unwrap()
                .as_ref(),
            Some(&trend.end.value)
        );
        let svg =
            render_svg_with_model(&artifact, recipe, cutoff + 86400, trend.end.value).unwrap();
        assert!(svg.contains(&format!("data-method=\"{}\"", trend.method)));
        assert!(svg.contains(&format!("data-forecast-r=\"{}\"", trend.end.value)));
        assert!(svg.contains("Ретроспективная модельная оценка"));
        assert!(svg.contains("id=\"observation-cutoff\"") && svg.contains("class=\"grid-day\""));
        assert!(svg.contains("id=\"raw-close-label\""));
        assert!(!svg.contains("Закрытие сигнала"));
        let grid_first = attr_after(&svg, r#"<line class="grid-day" x1=""#);
        let date_first = attr_after(&svg, r#"<text class="date" x=""#);
        assert!((grid_first - date_first).abs() < 0.05);
        let png = render_png_with_model(&artifact, cutoff + 86400, trend.end.value).unwrap();
        setups_relay_protocol::v2::validate_png(&png).unwrap();
        assert_eq!(
            png,
            render_png_with_model(&artifact, cutoff + 86400, trend.end.value).unwrap()
        );
        assert_eq!(
            render_png_with_model(&artifact, cutoff + 86400, trend.end.value + 0.01),
            Err(RenderError::ForecastMismatch)
        );
    }

    #[test]
    fn title_win_rate_aggregates_closed_counts_not_daily_percentages_or_unclosed_rows() {
        let mut artifact = with_outcomes(2.0, 1.0, &[(0, 1, 0), (3, 0, 9), (9, 0, 0)]);
        let MetricAvailability::Available { value: points } = &mut artifact.daily else {
            unreachable!()
        };
        points[0].row_count += 100;
        points[2].row_count += 100;
        artifact.validate().unwrap();
        assert_eq!(
            period_win_rate_pct(artifact.daily.as_ref().unwrap()),
            Some(10.0)
        );
        let svg = render_svg(&artifact).unwrap();
        assert!(svg.contains("-0.70 R</tspan> · Win rate: 10.0%</text>"));
        assert!(svg.contains(&format!(
            "fill=\"{FOREGROUND}\">{TITLE} · Среднее МО: <tspan fill=\"{NEGATIVE}\">"
        )));
        assert_eq!(period_win_rate_pct(&[]), None);
        let null = with_outcomes(2.0, 1.0, &[(0, 0, 0)]);
        assert_eq!(period_win_rate_pct(null.daily.as_ref().unwrap()), None);
        assert_eq!(render_png(&null), Err(RenderError::NoFiniteDays));
    }

    #[test]
    fn study_modes_share_matrix_solver_weights_bends_and_blend_formula() {
        use setups_domain::trend::TrendDesign;
        let artifact = curved_fixture();
        let sealed = artifact.to_json().unwrap();
        let points = artifact.daily.as_ref().unwrap();
        let mean = period_mean_r(&artifact, points);
        let mean_line = display_trend(points, mean, StudyTrend::default())
            .unwrap()
            .unwrap();
        assert_eq!(mean_line.method, "mean");
        assert!(mean_line.points.iter().all(|(_, value)| *value == mean));
        let input: Vec<_> = points
            .iter()
            .map(|point| TrendInput {
                day: point.x_utc_days,
                value: point.mean_r,
                weight: point.row_count as u32,
            })
            .collect();
        let mut endpoints = Vec::new();
        for bend in [
            TrendBendDays::Linear,
            TrendBendDays::Seven,
            TrendBendDays::Fourteen,
            TrendBendDays::Thirty,
        ] {
            let study = StudyTrend {
                trend_mode: TrendMode::Forecast,
                trend_bend_days: bend,
                ..StudyTrend::default()
            };
            let forecast = display_trend(points, mean, study).unwrap().unwrap();
            let matrix = TrendDesign::new(0.0, 42.0, bend)
                .unwrap()
                .endpoint(&input)
                .unwrap()
                .unwrap();
            assert_eq!(forecast.points.last().unwrap().1, matrix);
            assert_eq!(
                forecast.method,
                if bend == TrendBendDays::Linear {
                    "linear"
                } else {
                    "p_spline"
                }
            );
            assert!(forecast.points.len() <= 257);
            assert_eq!(forecast.points.first().unwrap().0, 0.0);
            assert_eq!(forecast.points.last().unwrap().0, 42.0);
            endpoints.push(matrix);
            for lambda in [
                TrendBlendLambda::default(),
                TrendBlendLambda::try_from(0.6).unwrap(),
            ] {
                let blend = display_trend(
                    points,
                    mean,
                    StudyTrend {
                        trend_mode: TrendMode::Blend,
                        trend_blend_lambda: lambda,
                        ..study
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(blend.points.len(), forecast.points.len());
                for (&(day, value), &(forecast_day, forecast_value)) in
                    blend.points.iter().zip(&forecast.points)
                {
                    assert_eq!(day, forecast_day);
                    assert_eq!(value, lambda.combine(mean, forecast_value));
                }
            }
        }
        assert!(
            endpoints
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() > 1e-6)
        );
        assert_eq!(
            artifact.to_json().unwrap(),
            sealed,
            "display fit never reseals scientific numbers"
        );
    }

    #[test]
    fn curves_are_open_unfilled_lines_with_no_confidence_band() {
        let artifact = curved_fixture();
        for mode in [TrendMode::Forecast, TrendMode::Blend] {
            let study = StudyTrend {
                trend_mode: mode,
                trend_bend_days: TrendBendDays::Seven,
                ..StudyTrend::default()
            };
            let svg = render_svg_with_trend(&artifact, study).unwrap();
            let start = svg.find("<polyline id=\"trend\"").unwrap();
            let line = &svg[start..start + svg[start..].find("/>").unwrap()];
            assert!(line.contains("data-method=\"p_spline\""));
            assert!(line.contains("fill=\"none\""));
            assert!(line.contains(&format!("stroke=\"{TREND}\"")));
            assert!(!line.contains(" Z") && !line.contains(" z"));
            assert!(
                !svg.contains("<polygon")
                    && !svg.contains("confidence")
                    && !svg.contains("fill-opacity")
            );
            assert!(!svg.contains(&format!("fill=\"{TREND}\" stroke-opacity")));
            assert_eq!(svg.matches("id=\"trend-start\"").count(), 1);
            assert_eq!(svg.matches("id=\"trend-end\"").count(), 1);
            let png = render_png_with_trend(&artifact, study).unwrap();
            setups_relay_protocol::v2::validate_png(&png).unwrap();
            assert_eq!(png, render_png_with_trend(&artifact, study).unwrap());
        }
    }

    #[test]
    fn axis_bounds_are_exact_display_extrema_clamped_to_zero_and_one() {
        for (artifact, minimum, maximum) in [
            (with_outcomes(0.2, 1.0, &[(0, 1, 0), (2, 1, 0)]), 0.0, 1.0),
            (with_outcomes(4.0, 1.0, &[(0, 1, 0), (2, 1, 1)]), 0.0, 4.0),
            (with_outcomes(2.0, 1.0, &[(0, 0, 1), (2, 0, 1)]), -1.0, 1.0),
            (with_outcomes(1.0, 1.0, &[(0, 1, 1), (2, 1, 1)]), 0.0, 1.0),
            (with_outcomes(4.0, 1.0, &[(0, 0, 1), (2, 1, 0)]), -1.0, 4.0),
        ] {
            let geometry = Geometry::from_points(artifact.daily.as_ref().unwrap(), None).unwrap();
            assert_eq!((geometry.y_min, geometry.y_max), (minimum, maximum));
            assert_eq!((geometry.y(maximum), geometry.y(minimum)), (TOP, BOTTOM));
            let svg = render_svg(&artifact).unwrap();
            assert!(svg.contains(&format!("id=\"axis-max\" x=\"70\" y=\"70.000\" text-anchor=\"end\" font-size=\"11\" fill=\"{AXIS}\">{}</text>", risk_label(maximum))));
            assert_eq!(svg.matches(">0</text>").count(), 1);
            if minimum < 0.0 {
                assert!(svg.contains(&format!("id=\"axis-min\" x=\"70\" y=\"262.000\" text-anchor=\"end\" font-size=\"11\" fill=\"{AXIS}\">{}</text>", risk_label(minimum))));
            }
        }
        // Internal curve extrema, not just endpoints, participate in the scale.
        let artifact = fixture();
        let curve = DisplayTrend {
            method: "p_spline",
            points: vec![(0.0, 0.2), (4.0, 8.0), (6.0, -3.0), (9.0, 0.5)],
        };
        let geometry =
            Geometry::from_points(artifact.daily.as_ref().unwrap(), Some(&curve)).unwrap();
        assert_eq!((geometry.y_min, geometry.y_max), (-3.0, 8.0));
    }

    #[test]
    fn saved_study_defaults_and_invalid_options_are_explicit_and_short_fits_fall_back() {
        let defaults: StudyTrend = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(defaults, StudyTrend::default());
        assert_eq!(defaults.trend_blend_lambda.fraction(), 0.7);
        for value in [
            serde_json::json!({"trendMode":"unknown"}),
            serde_json::json!({"trendBendDays":"8"}),
            serde_json::json!({"trendBlendLambda":0.705}),
            serde_json::json!({"trendBlendLambda":1}),
        ] {
            assert!(serde_json::from_value::<StudyTrend>(value).is_err());
        }
        let short = fixture();
        let points = short.daily.as_ref().unwrap();
        let curve = display_trend(
            points,
            0.8,
            StudyTrend {
                trend_mode: TrendMode::Forecast,
                trend_bend_days: TrendBendDays::Seven,
                ..defaults
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            curve.method, "linear",
            "three finite days cannot invent a spline"
        );
        let single = with_outcomes(2.0, 1.0, &[(0, 1, 0)]);
        for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
            let svg = render_svg_with_trend(
                &single,
                StudyTrend {
                    trend_mode: mode,
                    ..defaults
                },
            )
            .unwrap();
            assert!(!svg.contains("id=\"trend\""));
            assert!(svg.contains("id=\"no-trend\""));
        }
        let mut oversized = fixture();
        let MetricAvailability::Available { value: points } = &mut oversized.daily else {
            unreachable!()
        };
        points[0].row_count = u64::from(u32::MAX) + 1;
        oversized.validate().unwrap();
        assert_eq!(
            render_png_with_trend(
                &oversized,
                StudyTrend {
                    trend_mode: TrendMode::Forecast,
                    ..defaults
                }
            ),
            Err(RenderError::InvalidTrend(TrendError::InvalidDayOrValue)),
            "do not truncate oversized setup weights"
        );
    }

    #[test]
    fn utc_golden_uses_calendar_spacing_null_breaks_and_keeps_sealed_ols_unchanged() {
        let artifact = fixture();
        assert_eq!(artifact.origin_unix, ORIGIN);
        let cell = artifact.selected_cell.as_ref().unwrap();
        assert_eq!((cell.tp_count, cell.sl_count), (3, 2));
        assert_eq!(cell.raw_r.as_ref(), Some(&0.8));
        assert_eq!(cell.win_rate_pct.as_ref(), Some(&60.0));
        let trend = artifact.trend.unwrap();
        assert!((trend.slope_r_per_day - 1.0 / 6.0).abs() < 1e-12);
        let points = artifact.daily.as_ref().unwrap();
        let geometry = Geometry::from_points(points, None).unwrap();
        assert!((geometry.x(3.0) - LEFT - (RIGHT - LEFT) / 3.0).abs() < 1e-10);
        let svg = render_svg(&artifact).unwrap();
        assert_eq!(svg.matches("class=\"daily-point\"").count(), 3);
        assert_eq!(svg.matches("class=\"daily-segment\"").count(), 1);
        assert_eq!(svg.matches(">+0.80 R</text>").count(), 2);
        assert!(svg.contains(&format!(
            "{TITLE} · Среднее МО: <tspan fill=\"{POSITIVE}\">+0.80 R</tspan> · Win rate: 60.0%</text>"
        )));
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
    fn study_series_exposes_period_mean_and_fitted_trend() {
        let artifact = fixture();
        let series = study_series(&artifact, StudyTrend::default()).unwrap();
        assert!(series.mean_r.is_finite());
        assert!(series.trend_method.is_some());
        assert!(series.trend_points.len() >= 2);
        let rerendered = study_series(&artifact, StudyTrend::default()).unwrap();
        assert_eq!(series.mean_r, rerendered.mean_r);
        assert_eq!(series.trend_points, rerendered.trend_points);
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
            count_color(&pixels, [24, 207, 152], (100, 5, 800, 35)) > 20,
            "positive period mean colors the value in the title green"
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
                .contains("+0.80 R</tspan> · Win rate: 60.0%</text>")
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
                "fill=\"{FOREGROUND}\">{TITLE} · Среднее МО: <tspan fill=\"{color}\">{label}</tspan> · Win rate:"
            )));
            let geometry = Geometry::from_points(artifact.daily.as_ref().unwrap(), None).unwrap();
            assert!(svg.contains(&format!(
                "id=\"zero-label\" x=\"70\" y=\"{:.3}\"",
                geometry.y(0.0) + 4.0
            )));
            assert_eq!(svg.matches(">0</text>").count(), 1);
            let (_, pixels) = decoded(&render_png(&artifact).unwrap());
            assert!(count_color(&pixels, rgb, (100, 5, 800, 35)) > 20);
        }
    }

    #[test]
    fn outlier_fit_is_included_in_scale_and_both_endpoint_labels_are_visible() {
        let artifact = with_outcomes(10.0, 1.0, &[(0, 1, 10), (1, 1, 0), (2, 1, 0)]);
        let study = StudyTrend {
            trend_mode: TrendMode::Forecast,
            ..StudyTrend::default()
        };
        let points = artifact.daily.as_ref().unwrap();
        let trend = display_trend(points, period_mean_r(&artifact, points), study)
            .unwrap()
            .unwrap();
        let start_r = trend.points.first().unwrap().1;
        let end_r = trend.points.last().unwrap().1;
        assert!(end_r > 10.0);
        let geometry = Geometry::from_points(points, Some(&trend)).unwrap();
        assert_eq!(geometry.y_max, end_r);
        let svg = render_svg_with_trend(&artifact, study).unwrap();
        assert!(svg.contains("id=\"trend-start\"") && svg.contains("id=\"trend-end\""));
        assert!(svg.contains(&format!("{}</text>", risk_label(start_r))));
        assert!(svg.contains(&format!("{}</text>", risk_label(end_r))));
        let bytes = render_png_with_trend(&artifact, study).unwrap();
        let (_, pixels) = decoded(&bytes);
        for (x, value) in [(LEFT, start_r), (RIGHT - LABEL_WIDTH, end_r)] {
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
        for study in [
            StudyTrend::default(),
            StudyTrend {
                trend_mode: TrendMode::Forecast,
                trend_bend_days: TrendBendDays::Seven,
                ..StudyTrend::default()
            },
            StudyTrend {
                trend_mode: TrendMode::Blend,
                trend_bend_days: TrendBendDays::Thirty,
                ..StudyTrend::default()
            },
        ] {
            assert!(render_svg_with_trend(&artifact, study).unwrap().len() <= MAX_SVG_BYTES);
            let bytes = render_png_with_trend(&artifact, study).unwrap();
            assert!(bytes.len() <= MAX_PNG_BYTES);
            setups_relay_protocol::v2::validate_png(&bytes).unwrap();
        }
        let recipe = ForecastRecipe::engineering_baseline();
        let artifact = explicit_fixture(artifact, recipe);
        let cutoff = artifact.source.provenance.observation_cutoff;
        let value = probability_forecast(&artifact, recipe)
            .unwrap()
            .as_ref()
            .unwrap()
            .forecast_r;
        let svg = render_svg_with_probability(&artifact, recipe, cutoff + 60, value).unwrap();
        assert!(svg.len() <= MAX_SVG_BYTES);
        assert_eq!(svg.matches("class=\"daily-point\"").count(), 3661);
        let bytes = render_png_with_probability(&artifact, recipe, cutoff + 60, value).unwrap();
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
