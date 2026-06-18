//! Ring event types and parser for the operation characteristic.

/// Events emitted by the WIZPR Ring during operation.
#[derive(Debug, Clone, PartialEq)]
pub enum RingEvent {
    MicOn,
    MicOff,
    RecordingStarted,
    RecordingStopped,
    Click,
    DoubleClick,
    PowerOff,
    BatteryUpdate {
        voltage: f32,
        level: u8,
    },
    /// Any operation string we did not recognize. This is exposed for
    /// logging and diagnostics, not as a stable firmware-control API.
    Operation {
        raw: String,
    },
}

// Based on the latest SpokenWordWithBLE battery discharge curve.
#[allow(clippy::approx_constant)]
const BATTERY_VOLTAGE_TABLE: &[(f32, u8)] = &[
    (3.740, 100),
    (3.730, 97),
    (3.720, 95),
    (3.710, 94),
    (3.700, 91),
    (3.690, 90),
    (3.680, 87),
    (3.670, 85),
    (3.660, 84),
    (3.650, 81),
    (3.640, 78),
    (3.630, 77),
    (3.620, 75),
    (3.610, 72),
    (3.600, 71),
    (3.590, 68),
    (3.580, 67),
    (3.570, 65),
    (3.560, 62),
    (3.550, 61),
    (3.540, 58),
    (3.530, 56),
    (3.520, 54),
    (3.510, 52),
    (3.500, 49),
    (3.490, 48),
    (3.480, 46),
    (3.470, 44),
    (3.460, 42),
    (3.450, 39),
    (3.440, 38),
    (3.430, 37),
    (3.420, 35),
    (3.410, 34),
    (3.400, 32),
    (3.390, 30),
    (3.380, 29),
    (3.370, 28),
    (3.360, 27),
    (3.350, 25),
    (3.340, 24),
    (3.330, 22),
    (3.320, 19),
    (3.310, 16),
    (3.300, 15),
    (3.290, 13),
    (3.280, 13),
    (3.270, 11),
    (3.260, 11),
    (3.250, 11),
    (3.240, 10),
    (3.230, 10),
    (3.220, 10),
    (3.210, 10),
    (3.200, 9),
    (3.190, 9),
    (3.180, 9),
    (3.170, 9),
    (3.160, 8),
    (3.150, 8),
    (3.140, 8),
    (3.130, 8),
    (3.120, 6),
    (3.110, 6),
    (3.100, 6),
    (3.090, 6),
    (3.080, 5),
    (3.070, 5),
    (3.060, 5),
    (3.050, 5),
    (3.040, 4),
    (3.030, 4),
    (3.020, 4),
    (3.010, 4),
    (3.000, 4),
    (2.990, 4),
    (2.980, 3),
    (2.970, 3),
    (2.960, 3),
    (2.950, 3),
    (2.940, 1),
    (2.930, 1),
    (2.920, 1),
    (2.910, 1),
    (2.900, 0),
];

/// Parse a single operation message from the ring.
///
/// Click vs double-click is *not* decided here — that requires a timer
/// and lives in the transport layer. This function only classifies the
/// well-known operation strings the firmware sends.
pub fn parse_operation(text: &str) -> RingEvent {
    if text.contains("POWER_OFF") {
        return RingEvent::PowerOff;
    }
    if text.contains("MIC_ON") {
        return RingEvent::MicOn;
    }
    if text.contains("MIC_OFF") {
        return RingEvent::MicOff;
    }

    if let Some(voltage) = parse_battery_voltage(text) {
        let level = battery_voltage_to_percent(voltage);
        return RingEvent::BatteryUpdate { voltage, level };
    }

    RingEvent::Operation {
        raw: text.to_string(),
    }
}

fn parse_battery_voltage(text: &str) -> Option<f32> {
    if let Some(idx) = text.find("BATT=") {
        return parse_float_prefix(&text[idx + "BATT=".len()..]);
    }

    if text.contains("BATTERY") {
        if let Some(open) = text.find('(') {
            let rest = &text[open + 1..];
            if let Some(close) = rest.find(')') {
                return rest[..close].trim().parse::<f32>().ok();
            }
        }
    }

    None
}

fn parse_float_prefix(text: &str) -> Option<f32> {
    let trimmed = text.trim_start();
    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| {
            if ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+') {
                None
            } else {
                Some(idx)
            }
        })
        .unwrap_or(trimmed.len());

    trimmed[..end].parse::<f32>().ok()
}

fn battery_voltage_to_percent(voltage: f32) -> u8 {
    if voltage <= 0.0 {
        return 0;
    }

    let (max_voltage, max_percent) = BATTERY_VOLTAGE_TABLE[0];
    if voltage >= max_voltage {
        return max_percent;
    }

    let (min_voltage, min_percent) = BATTERY_VOLTAGE_TABLE[BATTERY_VOLTAGE_TABLE.len() - 1];
    if voltage <= min_voltage {
        return min_percent;
    }

    for pair in BATTERY_VOLTAGE_TABLE.windows(2) {
        let (current_voltage, current_percent) = pair[0];
        let (next_voltage, next_percent) = pair[1];

        if voltage <= current_voltage && voltage >= next_voltage {
            let voltage_range = current_voltage - next_voltage;
            let percentage_range = f32::from(current_percent) - f32::from(next_percent);
            let voltage_offset = current_voltage - voltage;
            let interpolated =
                f32::from(current_percent) - (voltage_offset / voltage_range) * percentage_range;
            return interpolated.round().clamp(0.0, 100.0) as u8;
        }
    }

    50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_keywords() {
        assert_eq!(parse_operation("MIC_ON"), RingEvent::MicOn);
        assert_eq!(parse_operation("MIC_OFF"), RingEvent::MicOff);
        assert_eq!(parse_operation("POWER_OFF"), RingEvent::PowerOff);
    }

    #[test]
    fn battery_full_voltage_maps_to_100_percent() {
        let RingEvent::BatteryUpdate { voltage, level } = parse_operation("BATT=4.2") else {
            panic!("expected BatteryUpdate");
        };
        assert!((voltage - 4.2).abs() < 1e-3);
        assert_eq!(level, 100);
    }

    #[test]
    fn battery_table_voltage_maps_to_spokenword_curve() {
        let RingEvent::BatteryUpdate { voltage, level } = parse_operation("BATT=3.350") else {
            panic!("expected BatteryUpdate");
        };
        assert!((voltage - 3.350).abs() < 1e-3);
        assert_eq!(level, 25);
    }

    #[test]
    fn battery_three_volts_maps_to_spokenword_curve() {
        let RingEvent::BatteryUpdate { level, .. } = parse_operation("BATT=3.0") else {
            panic!("expected BatteryUpdate");
        };
        assert_eq!(level, 4);
    }

    #[test]
    fn below_min_voltage_clamps_to_zero() {
        let RingEvent::BatteryUpdate { level, .. } = parse_operation("BATT=2.5") else {
            panic!("expected BatteryUpdate");
        };
        assert_eq!(level, 0);
    }

    #[test]
    fn parses_spokenword_battery_percent_voltage_format() {
        let RingEvent::BatteryUpdate { voltage, level } = parse_operation("BATTERY 75(3.524322)")
        else {
            panic!("expected BatteryUpdate");
        };
        assert!((voltage - 3.524322).abs() < 1e-3);
        assert_eq!(level, 55);
    }

    #[test]
    fn parses_batt_voltage_with_trailing_text() {
        let RingEvent::BatteryUpdate { voltage, level } = parse_operation("BATT=3.524322 OK")
        else {
            panic!("expected BatteryUpdate");
        };
        assert!((voltage - 3.524322).abs() < 1e-3);
        assert_eq!(level, 55);
    }

    #[test]
    fn malformed_battery_string_remains_unparsed_operation() {
        let RingEvent::Operation { raw } = parse_operation("BATT=unknown") else {
            panic!("expected Operation");
        };
        assert_eq!(raw, "BATT=unknown");
    }

    #[test]
    fn unknown_strings_become_unparsed_operation() {
        let RingEvent::Operation { raw } = parse_operation("FUTURE_CODE") else {
            panic!("expected Operation");
        };
        assert_eq!(raw, "FUTURE_CODE");
    }
}
