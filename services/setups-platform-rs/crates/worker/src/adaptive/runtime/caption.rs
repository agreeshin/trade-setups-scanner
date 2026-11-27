//! Telegram-only rich presentation. The saved recommendation is read-only;
//! delivery/version/attachment selection belongs to the durable sender.
use super::*;
use chrono::{DateTime, Utc};
use setups_relay_protocol::v2::{self, EntityKind, TextEntity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveTelegramCaption {
    pub text: String,
    pub entities: Vec<TextEntity>,
    /// Safe bounded diagnostic for the sender/app, not user data or a stack.
    pub enrichment_unavailable_reason: Option<&'static str>,
}

impl AdaptiveTelegramCaption {
    /// False means one text-only publication must be selected BEFORE freezing
    /// a new job. Never truncate trade conditions or modify an attempted body.
    pub fn fits_photo_caption(&self) -> bool {
        v2::utf16_len(&self.text) <= v2::MAX_CAPTION_UTF16_UNITS
    }
}

/// Bounded per-call builder; owns at most 4096 UTF-16 units and 32 ranges.
/// Position is advanced with each append, so astral emoji never shift entities.
#[derive(Default)]
struct CaptionBuilder {
    text: String,
    entities: Vec<TextEntity>,
    units: usize,
}

impl CaptionBuilder {
    fn push(&mut self, text: &str, kind: Option<EntityKind>) -> Result<(), WorkerError> {
        let length = v2::utf16_len(text);
        let next = self
            .units
            .checked_add(length)
            .ok_or_else(|| invalid("caption length overflow"))?;
        if next > v2::MAX_TEXT_UTF16_UNITS {
            return Err(invalid("adaptive Telegram text exceeds text-only limit"));
        }
        if let Some(kind) = kind {
            if length == 0 || self.entities.len() >= v2::MAX_ENTITIES {
                return Err(invalid("adaptive Telegram entities exceed limit"));
            }
            self.entities.push(TextEntity {
                kind,
                offset: self.units as u32,
                length: length as u32,
            });
        }
        self.text.push_str(text);
        self.units = next;
        Ok(())
    }

    fn plain(&mut self, text: &str) -> Result<(), WorkerError> {
        self.push(text, None)
    }
    fn code(&mut self, text: &str) -> Result<(), WorkerError> {
        self.push(text, Some(EntityKind::Code))
    }
    fn bold(&mut self, text: &str) -> Result<(), WorkerError> {
        self.push(text, Some(EntityKind::Bold))
    }
    fn label(&mut self, text: &str) -> Result<(), WorkerError> {
        // One bounded range covers numbers inside literal user labels, without
        // allowing an arbitrary name to create unbounded per-number entities.
        if text.chars().any(char::is_numeric) {
            self.code(text)
        } else {
            self.plain(text)
        }
    }
    fn finish(self) -> Result<AdaptiveTelegramCaption, WorkerError> {
        v2::validate_entities(&self.text, &self.entities)
            .map_err(|_| invalid("invalid adaptive Telegram entities"))?;
        Ok(AdaptiveTelegramCaption {
            text: self.text,
            entities: self.entities,
            enrichment_unavailable_reason: None,
        })
    }
}

#[derive(Default)]
struct Metrics {
    forecast_r: Option<f64>,
    current: bool,
}

fn signed_r(value: f64) -> Result<String, WorkerError> {
    if !value.is_finite() {
        return Err(invalid("non-finite adaptive Telegram metric"));
    }
    Ok(format!("{value:+.2} R"))
}

fn validate_label(value: &str) -> Result<(), WorkerError> {
    if value.trim().is_empty()
        || value.chars().count() > 120
        || value.len() > 480
        || value.chars().any(char::is_control)
    {
        return Err(invalid("invalid adaptive Telegram label"));
    }
    Ok(())
}

/// Formats only frozen values. No clock, settings, current winner, market reads,
/// chart fitting or transport are consulted here. UTC is explicit and stable.
pub fn recommendation_telegram_caption(
    instrument: &AdaptiveInstrument,
    raw: &SpikeSetup,
    value: &Value,
) -> Result<AdaptiveTelegramCaption, WorkerError> {
    let envelope: RecommendationEnvelope = serde_json::from_value(value.clone())?;
    if value
        .get("rawSignalAt")
        .is_some_and(|at| at.as_i64() != Some(raw.close_time_unix))
    {
        return Err(invalid(
            "caption raw signal differs from saved recommendation",
        ));
    }
    let (metrics, diagnostic) = match value.get("expectancy") {
        Some(value) => {
            match serde_json::from_value::<super::enrichment::AdaptiveExpectancySnapshot>(
                value.clone(),
            ) {
                Ok(saved) if saved.validate_for(&envelope, raw.close_time_unix).is_ok() => (
                    Metrics {
                        forecast_r: saved.objective.as_ref().map_or_else(
                            || saved.forecast_r.as_ref().copied(),
                            |objective| objective.decision_r.as_ref().copied(),
                        ),
                        current: false,
                    },
                    if saved.image_key.is_none() {
                        Some("saved_chart_unavailable")
                    } else {
                        None
                    },
                ),
                _ => (Metrics::default(), Some("invalid_saved_enrichment")),
            }
        }
        None => (Metrics::default(), Some("saved_enrichment_unavailable")),
    };
    let mut caption = format_caption(instrument, raw, &envelope, metrics)?;
    caption.enrichment_unavailable_reason = diagnostic;
    Ok(caption)
}

/// Explicit prospective contract only. Its objective and PNG are an admission
/// snapshot at fixed C, not a raw-signal forecast or a current-profile lookup.
pub fn recommendation_current_telegram_caption(
    instrument:&AdaptiveInstrument,raw:&SpikeSetup,value:&Value,current:&Value,
) -> Result<AdaptiveTelegramCaption,WorkerError> {
    let envelope:RecommendationEnvelope=serde_json::from_value(value.clone())?;
    if value["rawSignalAt"].as_i64()!=Some(raw.close_time_unix) {return Err(invalid("current caption signal mismatch"));}
    let expected=setups_domain::expectancy::ExpectancyObjective::from_recommendation(&envelope.snapshot)
        .map_err(|_|invalid("current caption objective invalid"))?;
    let snapshot=&current["snapshot"];
    let valid=current["status"]=="available" && snapshot["revision"].as_i64().is_some_and(|r|r>0)
        && snapshot["studyId"].as_i64()==Some(envelope.study_id) && snapshot["generationId"].as_i64()==Some(envelope.generation_id)
        && snapshot["tpPct"].as_f64()==Some(envelope.snapshot.tp_pct) && snapshot["slPct"].as_f64()==Some(envelope.snapshot.sl_pct)
        && snapshot["originalSnapshotHash"]==envelope.snapshot.provenance.snapshot_hash && snapshot["objective"]==serde_json::to_value(expected)?
        && snapshot["observationCutoff"].as_i64().is_some_and(|c|c>=envelope.snapshot.provenance.observation_cutoff)
        && snapshot["selectedObjectiveR"].as_f64().is_some_and(f64::is_finite);
    let mut caption=format_caption(instrument,raw,&envelope,Metrics {forecast_r:valid.then(||snapshot["selectedObjectiveR"].as_f64()).flatten(),current:true})?;
    if !valid {caption.enrichment_unavailable_reason=Some(if current["status"]=="unavailable" {"saved_chart_unavailable"}else{"invalid_saved_enrichment"});}
    Ok(caption)
}

fn format_caption(
    instrument: &AdaptiveInstrument,
    raw: &SpikeSetup,
    envelope: &RecommendationEnvelope,
    metrics: Metrics,
) -> Result<AdaptiveTelegramCaption, WorkerError> {
    let snapshot = &envelope.snapshot;
    validate_label(&instrument.symbol)?;
    validate_label(&instrument.exchange)?;
    validate_label(&envelope.external_name)?;
    if !(0..=43200).contains(&snapshot.time_shift_minutes)
        || ![
            snapshot.tp_pct,
            snapshot.sl_pct,
            snapshot.raw_r,
            snapshot.mean_r,
        ]
        .iter()
        .all(|x| x.is_finite())
        || !(0.0 < snapshot.tp_pct && snapshot.tp_pct <= 100.0)
        || !(0.0 < snapshot.sl_pct && snapshot.sl_pct <= 100.0)
    {
        return Err(invalid("invalid adaptive Telegram recommendation values"));
    }
    let target = raw
        .close_time_unix
        .checked_add(
            snapshot
                .time_shift_minutes
                .checked_mul(60)
                .ok_or_else(|| invalid("entry time overflow"))?,
        )
        .ok_or_else(|| invalid("entry time overflow"))?;
    let mut out = CaptionBuilder::default();
    let short = snapshot.direction == RecommendationDirection::Short;
    out.plain(if short { "🔴 " } else { "🟢 " })?;
    out.code(&instrument.symbol)?;
    out.plain(if short {
        " · SHORT · "
    } else {
        " · LONG · "
    })?;
    out.label(if instrument.exchange == "bybit" {
        "Bybit"
    } else {
        &instrument.exchange
    })?;
    out.plain("\nЦена: ")?;
    // Reuse the bounded decimal helpers; no price ever passes through f64.
    out.code(&super::text::display_decimal(&raw.close_price)?.replace(',', "."))?;
    out.plain(" USDT\n\n")?;
    if let Some(forecast) = metrics.forecast_r {
        out.plain(if forecast < 0.0 { "📉 " } else { "📈 " })?;
        out.plain(if metrics.current {"Прогноз матожидания торговли: "}else{"Матожидание торговли: "})?;
        out.code(&signed_r(forecast)?)?;
        out.plain("\n\n")?;
    }
    out.plain("Условия сделки:")?;
    if snapshot.time_shift_minutes > 0 {
        let time = DateTime::<Utc>::from_timestamp(target, 0)
            .ok_or_else(|| invalid("invalid entry timestamp"))?;
        out.plain("\n⏳ Рекомендуемое время входа: ")?;
        out.code(&format!("{} UTC", time.format("%H:%M:%S")))?;
    }
    out.plain("\n🎯 TP = ")?;
    out.code(&format!("{}%", snapshot.tp_pct))?;
    out.plain(", SL = ")?;
    out.code(&format!("{}%", snapshot.sl_pct))?;
    out.plain(" (от фактической цены входа)")?;
    if let Some(pct) = snapshot.price_change_pct {
        let threshold = super::text::price_threshold(&raw.close_price, pct, snapshot.direction)?
            .replace(',', ".");
        out.plain(if short {
            "\n⚠️ Заходить в сделку, если цена будет выше "
        } else {
            "\n⚠️ Заходить в сделку, если цена будет ниже "
        })?;
        out.code(&threshold)?;
        out.plain(" USDT на момент входа")?;
    }
    out.plain("\n\n")?;
    out.bold("Алгоритм")?;
    out.plain(": ")?;
    out.label(&envelope.external_name)?;
    let study_day = chrono::DateTime::<chrono::Utc>::from_timestamp(
        envelope.snapshot.provenance.observation_cutoff,
        0,
    )
    .map(|at| at.format("%d.%m.%Y").to_string())
    .unwrap_or_default();
    out.plain(&format!(
        "\nИсследование №{} от {study_day}",
        envelope.study_id
    ))?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::AlgorithmParams;

    fn fixture() -> (AdaptiveInstrument, SpikeSetup, Value) {
        let raw = SpikeSetup {
            algorithm_id: "price_volume_spike".into(),
            algorithm_version: "1.1.1".into(),
            open_time_unix: 86280,
            close_time_unix: 86340,
            close_price: "100".into(),
            params: AlgorithmParams::default(),
            details: None,
        };
        let instrument = AdaptiveInstrument {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "SKYUSDT".into(),
        };
        let value = json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
            "timeShiftMinutes":3,"priceChangePct":1,"clusterEnabled":true,"clusterGapMinutes":480,
            "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":"plan-a-continuous-1",
                "seed":42,"snapshotHash":"fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":0,"toUnix":86400,"observationCutoff":86000},
            "studyId":42,"generationId":3,"slotId":7,"ownerId":"admin","externalName":"<b>*plain name*😀","params":raw.params,
            "rawSignalAt":86340,"recommendedEntryAt":86520});
        (instrument, raw, value)
    }

    fn entity_text(caption: &AdaptiveTelegramCaption, entity: &TextEntity) -> String {
        String::from_utf16(
            &caption
                .text
                .encode_utf16()
                .skip(entity.offset as usize)
                .take(entity.length as usize)
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn current_caption_uses_admitted_objective_and_original_footer_even_when_cutoff_is_after_signal() {
        let (instrument,raw,value)=fixture();
        let original=value.clone();
        let legacy=recommendation_telegram_caption(&instrument,&raw,&value).unwrap();
        let recommendation:setups_domain::AdaptiveRecommendationSnapshot=serde_json::from_value(value.clone()).unwrap();
        let objective=setups_domain::expectancy::ExpectancyObjective::from_recommendation(&recommendation).unwrap();
        let mut current=json!({"status":"available","snapshot":{"revision":2,"source":"actual","sourceRevision":7,
            "studyId":42,"generationId":3,"observationCutoff":3*86400,"tpPct":14.0,"slPct":4.0,
            "originalSnapshotHash":"fixture","objective":objective,"selectedObjectiveR":0.4}});
        for (r,line) in [(0.4,"📈 Прогноз матожидания торговли: +0.40 R"),(-0.4,"📉 Прогноз матожидания торговли: -0.40 R"),
            (0.0,"📈 Прогноз матожидания торговли: +0.00 R"),(-0.001,"📉 Прогноз матожидания торговли: -0.00 R")] {
            current["snapshot"]["selectedObjectiveR"]=json!(r);
            let caption=recommendation_current_telegram_caption(&instrument,&raw,&value,&current).unwrap();
            assert!(caption.text.contains(line));assert_eq!(caption.enrichment_unavailable_reason,None);
            assert!(caption.text.ends_with("Исследование №42 от 01.01.1970"));
            assert!(caption.text.contains("TP = 14%, SL = 4%"));
            v2::validate_entities(&caption.text,&caption.entities).unwrap();
            assert!(caption.entities.iter().any(|entity|entity.kind==EntityKind::Code && entity_text(&caption,entity)==signed_r(r).unwrap()));
        }
        for (key,invalid) in [("tpPct",json!(15)),("generationId",json!(999)),("originalSnapshotHash",json!("other")),
            ("observationCutoff",json!(85999)),("objective",json!({}))] {
            let mut bad=current.clone();bad["snapshot"][key]=invalid;
            let caption=recommendation_current_telegram_caption(&instrument,&raw,&value,&bad).unwrap();
            assert_eq!(caption.enrichment_unavailable_reason,Some("invalid_saved_enrichment"));assert!(!caption.text.contains("матожидания торговли"));
            assert!(caption.text.contains("Условия сделки:"));
        }
        assert_eq!(value,original);assert_eq!(recommendation_telegram_caption(&instrument,&raw,&value).unwrap(),legacy);
        let mut wrong_raw=value.clone();wrong_raw["rawSignalAt"]=json!(raw.close_time_unix+60);
        assert!(recommendation_current_telegram_caption(&instrument,&raw,&wrong_raw,&current).is_err());
    }

    #[test]
    fn search_scores_and_period_means_never_substitute_for_saved_signal_forecast() {
        let (instrument, raw, mut value) = fixture();
        let expected = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        for search in [
            json!({"bendDays":"7","cellR":0.61,"matrixR":0.45}),
            json!({"bendDays":"linear","cellR":0.61,"matrixR":0.45}),
            json!({"bendDays":"linear","cellR":0.61,"matrixR":0.45,
                "blend":{"lambda":0.7,"cellR":0.52,"matrixR":0.38}}),
        ] {
            value["searchForecast"] = search;
            let before = value.clone();
            let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
            assert_eq!(value, before);
            assert_eq!(caption, expected);
            assert!(!caption.text.contains("Прогноз"));
        }
    }

    #[test]
    fn rich_caption_has_exact_labels_utf16_entities_literal_names_and_no_study_or_date() {
        let (instrument, raw, value) = fixture();
        let before = value.clone();
        let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert_eq!(before, value);
        assert!(
            caption
                .text
                .starts_with("🔴 SKYUSDT · SHORT · Bybit\nЦена: 100.00 USDT")
        );
        assert!(
            caption
                .text
                .contains("⏳ Рекомендуемое время входа: 00:02:00 UTC\n🎯 TP")
        );
        assert!(!caption.text.contains("мин после сетапа"));
        assert!(
            caption
                .text
                .contains("🎯 TP = 14%, SL = 4% (от фактической цены входа)")
        );
        assert!(
            caption
                .text
                .contains("⚠️ Заходить в сделку, если цена будет выше 101.00 USDT")
        );
        assert!(
            caption
                .text
                .contains("Цена: 100.00 USDT\n\nУсловия сделки:")
        );
        assert!(!caption.text.contains("Прогноз"));
        assert!(!caption.text.contains("Матожидание торговли"));
        assert_no_other_metrics(&caption.text);
        assert!(
            caption
                .text
                .ends_with("Алгоритм: <b>*plain name*😀\nИсследование №42 от 01.01.1970")
        );
        assert!(!caption.text.contains("1970-"));
        let headings: Vec<_> = caption
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Bold)
            .map(|entity| entity_text(&caption, entity))
            .collect();
        assert_eq!(headings, ["Алгоритм"]);
        assert_eq!(caption.entities[0].offset, 3); // red circle = two UTF-16 units + space
        assert_eq!(entity_text(&caption, &caption.entities[0]), "SKYUSDT");
        assert!(caption.fits_photo_caption());
        v2::validate_entities(&caption.text, &caption.entities).unwrap();
    }

    #[test]
    fn forecast_icon_uses_unrounded_sign_and_keeps_metric_entities_exact() {
        let (instrument, raw, value) = fixture();
        let envelope: RecommendationEnvelope = serde_json::from_value(value).unwrap();
        for (forecast, expected) in [
            (Some(0.4), "📈 Матожидание торговли: +0.40 R"),
            (Some(-0.4), "📉 Матожидание торговли: -0.40 R"),
            (Some(0.0), "📈 Матожидание торговли: +0.00 R"),
            (Some(-0.001), "📉 Матожидание торговли: -0.00 R"),
            (None, "Цена: 100.00 USDT\n\nУсловия сделки:"),
        ] {
            let caption = format_caption(
                &instrument,
                &raw,
                &envelope,
                Metrics {
                    forecast_r: forecast,
                    ..Metrics::default()
                },
            )
            .unwrap();
            assert!(caption.text.contains(expected));
            assert_no_other_metrics(&caption.text);
            assert!(!caption.text.contains("\n\n\n"));
            assert!(!caption.text.contains("мин после сетапа"));
            assert!(!caption.text.contains("только если"));
            v2::validate_entities(&caption.text, &caption.entities).unwrap();
            if let Some(value) = forecast {
                assert_eq!(caption.text.matches("Матожидание торговли:").count(), 1);
                assert!(
                    caption
                        .text
                        .contains(&format!("USDT\n\n{expected}\n\nУсловия сделки:"))
                );
                assert!(
                    caption
                        .entities
                        .iter()
                        .any(|entity| entity.kind == EntityKind::Code
                            && entity_text(&caption, entity) == signed_r(value).unwrap())
                );
            } else {
                assert!(!caption.text.contains("Прогноз"));
                assert!(!caption.text.contains('📈') && !caption.text.contains('📉'));
            }
            assert!(
                caption
                    .entities
                    .iter()
                    .any(|entity| entity.kind == EntityKind::Code
                        && entity_text(&caption, entity) == "00:02:00 UTC")
            );
        }
    }

    #[test]
    fn rich_caption_reuses_exact_signed_gate_math_in_both_directions_and_omits_null_shift_zero() {
        let (instrument, mut raw, mut value) = fixture();
        for (direction, pct, expected) in [
            ("short", 1.0, "101.00"),
            ("short", -1.0, "99.00"),
            ("long", 1.0, "99.00"),
            ("long", -1.0, "101.00"),
            ("long", 0.0, "100.00"),
        ] {
            value["direction"] = json!(direction);
            value["priceChangePct"] = json!(pct);
            let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
            assert!(
                caption
                    .text
                    .contains(&format!("{expected} USDT на момент входа"))
            );
            assert!(caption.text.contains(if direction == "short" {
                "🔴 SKYUSDT · SHORT"
            } else {
                "🟢 SKYUSDT · LONG"
            }));
        }
        value["priceChangePct"] = Value::Null;
        value["timeShiftMinutes"] = json!(0);
        let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert!(!caption.text.contains("⏳") && !caption.text.contains("⚠️"));
        raw.close_price = "123456789012345678.123456789012345678".into();
        value["direction"] = json!("short");
        value["priceChangePct"] = json!(1);
        let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert!(
            caption
                .text
                .contains("124691356902469134.90469135690246913478")
        );
        raw.close_price = "100".into();
        value["priceChangePct"] = json!(1e-7);
        assert!(
            recommendation_telegram_caption(&instrument, &raw, &value)
                .unwrap()
                .text
                .contains("100.0000001")
        );
    }

    #[test]
    fn max_emoji_name_and_metrics_preserve_safe_bounded_text_without_truncating_conditions() {
        let (instrument, raw, mut value) = fixture();
        value["externalName"] = json!("😀".repeat(120));
        let envelope: RecommendationEnvelope = serde_json::from_value(value).unwrap();
        let caption = format_caption(
            &instrument,
            &raw,
            &envelope,
            Metrics {
                forecast_r: Some(0.4),
                ..Metrics::default()
            },
        )
        .unwrap();
        assert!(caption.text.contains("📈 Матожидание торговли: +0.40 R"));
        assert_no_other_metrics(&caption.text);
        assert!(
            caption
                .text
                .ends_with(&format!("{}\nИсследование №42 от 01.01.1970", "😀".repeat(120)))
        );
        v2::validate_entities(&caption.text, &caption.entities).unwrap();
        let mut builder = CaptionBuilder::default();
        builder
            .plain(&"a".repeat(v2::MAX_CAPTION_UTF16_UNITS + 1))
            .unwrap();
        let long = builder.finish().unwrap();
        assert!(!long.fits_photo_caption());
        assert_eq!(long.text.len(), v2::MAX_CAPTION_UTF16_UNITS + 1);
        let mut builder = CaptionBuilder::default();
        assert!(
            builder
                .plain(&"a".repeat(v2::MAX_TEXT_UTF16_UNITS + 1))
                .is_err()
        );
    }

    #[test]
    fn digits_in_literal_names_are_code_and_saved_scalar_corruption_only_degrades() {
        let (instrument, raw, mut value) = fixture();
        value["externalName"] = json!("😀<b>algo42</b>*literal*");
        value["expectancy"] = json!({"corrupt":true});
        let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert_eq!(
            caption.enrichment_unavailable_reason,
            Some("invalid_saved_enrichment")
        );
        assert!(!caption.text.contains("Прогноз"));
        assert!(!caption.text.contains("Матожидание торговли"));
        assert_no_other_metrics(&caption.text);
        let name = caption.entities.iter().rev().next().unwrap();
        assert_eq!(name.kind, EntityKind::Code);
        assert_eq!(entity_text(&caption, name), "😀<b>algo42</b>*literal*");
        value["rawSignalAt"] = json!(raw.close_time_unix + 60);
        assert!(recommendation_telegram_caption(&instrument, &raw, &value).is_err());
    }

    fn assert_no_other_metrics(text: &str) {
        for forbidden in [
            "Планируемое",
            "Среднее МО",
            "Динамика МО",
            "винрейт",
            "В среднем",
            "матриц",
            "Выбор по",
            "🔖",
        ] {
            assert!(!text.contains(forbidden), "unexpected {forbidden}: {text}");
        }
    }

    #[test]
    fn saved_objective_mean_is_the_only_metric_and_target_template_is_exact() {
        let (mut instrument, mut raw, mut value) = fixture();
        instrument.symbol = "SPACEUSDT".into();
        raw.close_price = "0.004156".into();
        raw.close_time_unix = 18 * 3600 + 24 * 60;
        value["rawSignalAt"] = json!(raw.close_time_unix);
        value["tpPct"] = json!(20);
        value["slPct"] = json!(6);
        value["priceChangePct"] = json!(0);
        value["externalName"] = json!("Всплеск цены и оборота");
        // The source research is older than this signal, not ahead of it.
        value["provenance"]["observationCutoff"] = json!(raw.close_time_unix);
        let envelope = serde_json::from_value(value.clone()).unwrap();
        let mut saved =
            super::super::enrichment::freeze(&envelope, raw.close_time_unix, None).unwrap();
        saved.forecast_r = setups_domain::expectancy::MetricAvailability::Available { value: 0.13 };
        value["expectancy"] = serde_json::to_value(saved).unwrap();
        let before = value.clone();
        let caption = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert_eq!(value, before);
        assert_eq!(
            caption.text,
            concat!(
                "🔴 SPACEUSDT · SHORT · Bybit\nЦена: 0.004156 USDT\n\n",
                "📈 Матожидание торговли: +0.32 R\n\n",
                "Условия сделки:\n⏳ Рекомендуемое время входа: 18:27:00 UTC\n",
                "🎯 TP = 20%, SL = 6% (от фактической цены входа)\n",
                "⚠️ Заходить в сделку, если цена будет выше 0.004156 USDT на момент входа\n\n",
                "Алгоритм: Всплеск цены и оборота\nИсследование №42 от 01.01.1970"
            )
        );
        assert!(caption.fits_photo_caption());
        let bold: Vec<_> = caption
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Bold)
            .map(|e| entity_text(&caption, e))
            .collect();
        assert_eq!(bold, ["Алгоритм"]);
        v2::validate_entities(&caption.text, &caption.entities).unwrap();
        // A matching timestamp is required; never silently use an unrelated forecast.
        value["expectancy"]["forecastAtUnix"] = json!(raw.close_time_unix + 60);
        let invalid = recommendation_telegram_caption(&instrument, &raw, &value).unwrap();
        assert!(!invalid.text.contains("Прогноз"));
        assert_eq!(
            invalid.enrichment_unavailable_reason,
            Some("invalid_saved_enrichment")
        );
    }
}
