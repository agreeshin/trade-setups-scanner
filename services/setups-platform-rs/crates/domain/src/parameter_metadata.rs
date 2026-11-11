//! The module-owned parameter definitions used by both controls and optimizer.
use crate::{SPIKE_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_ID};
use serde_json::{Value, json};

pub fn algorithm_fields(id: &str) -> Option<Value> {
    let (price_key, price_label, price_max, window_label) = match id {
        SPIKE_ALGORITHM_ID => (
            "priceGrowthPct",
            "Рост цены",
            1000,
            "Окно роста цены и оборота",
        ),
        SPIKE_DROP_ALGORITHM_ID => (
            "priceDropPct",
            "Падение цены",
            100,
            "Окно падения цены и оборота",
        ),
        _ => return None,
    };
    Some(json!([
        { "key": price_key, "label": price_label, "type": "decimal", "min": 0, "max": price_max, "unit": "%",
          "optimization": {"enabledByDefault":true,"values":["4","8","12","16","20"]} },
        { "key": "windowMinutes", "label": window_label, "type": "integer", "min": 1, "max": 1440, "unit": "мин",
          "optimization": {"enabledByDefault":true,"values":[3,5,10,15]} },
        { "key": "volumeMultiplier", "label": "Множитель оборота", "type": "decimal", "min": 0.000001, "max": 1000, "unit": "×",
          "optimization": {"enabledByDefault":true,"values":["2","3","4","6"]} },
        { "key": "baselineLongDays", "label": "Длинная база", "type": "integer", "min": 1, "max": 90, "unit": "дней",
          "optimization": {"enabledByDefault":true,"values":[14,30,60]} },
        { "key": "baselineShortDays", "label": "Короткая база", "type": "integer", "min": 1, "max": 90, "unit": "дней",
          "optimization": {"enabledByDefault":true,"values":[3,7,14]} },
        { "key": "cooldownMinutes", "label": "Интервал между сетапами", "type": "integer", "min": 0, "max": 10080, "unit": "мин",
          "optimization": {"enabledByDefault":true,"values":[0,5,15,60]} },
        { "key": "maxGapPercent", "label": "Допустимая доля пропусков", "type": "decimal", "min": 0, "max": 100, "unit": "%",
          "optimization": {"enabledByDefault":false,"values":["0","1","2"]} },
        { "key": "maxConsecutiveGapMinutes", "label": "Максимальный непрерывный пропуск", "type": "integer", "min": 0, "max": 1440, "unit": "мин",
          "optimization": {"enabledByDefault":false,"values":[0,5,10]} },
        { "key": "requiredRecentMinutes", "label": "Последние минуты без пропусков", "type": "integer", "min": 0, "max": 1440, "unit": "мин",
          "optimization": {"enabledByDefault":false,"values":[5,10,30]} }
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpikeDropParams, SpikeParams};
    use std::collections::BTreeSet;

    #[test]
    fn metadata_matches_the_actual_serialized_parameters_of_each_module() {
        for (id, params) in [
            (
                SPIKE_ALGORITHM_ID,
                serde_json::to_value(SpikeParams::default()).unwrap(),
            ),
            (
                SPIKE_DROP_ALGORITHM_ID,
                serde_json::to_value(SpikeDropParams::default()).unwrap(),
            ),
        ] {
            let fields = algorithm_fields(id).unwrap();
            let fields = fields.as_array().unwrap();
            let keys: BTreeSet<_> = fields
                .iter()
                .map(|field| field["key"].as_str().unwrap())
                .collect();
            assert_eq!(
                keys,
                params
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect()
            );
            assert_eq!(keys.len(), fields.len());
            for field in fields {
                let key = field["key"].as_str().unwrap();
                assert_eq!(field["type"] == "decimal", params[key].is_string());
                assert!(field["min"].is_number() && field["max"].is_number());
                assert!(
                    field["optimization"]["values"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty())
                );
                for value in field["optimization"]["values"].as_array().unwrap() {
                    assert_eq!(value.is_string(), params[key].is_string());
                }
            }
        }
        assert!(algorithm_fields("not_registered").is_none());
    }
}
