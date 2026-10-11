-- All displayed times are UTC. These queries are read-only.

-- Recent total power and the persistent pin 5 / pin 6 imbalance.
SELECT datetime(unix_time_ms / 1000.0, 'unixepoch') AS utc,
       round(connector_watts, 1) AS connector_watts,
       round(max_pin_amps, 2) AS highest_pin_amps,
       round(p5_amps, 2) AS pin5_amps,
       round(p6_amps, 2) AS pin6_amps,
       round(p5_amps / nullif(p6_amps, 0), 2) AS pin5_to_pin6
FROM samples ORDER BY unix_time_ms DESC LIMIT 30;

-- Highest and average current on each pin in the retained history.
SELECT pin, round(max(amps), 3) AS peak_amps,
       round(avg(amps), 3) AS average_amps, count(*) AS samples
FROM pin_readings GROUP BY pin ORDER BY pin;

-- Warning-range samples; the 9.2 A level is a display/logging warning.
SELECT datetime(unix_time_ms / 1000.0, 'unixepoch') AS utc,
       pin, round(amps, 3) AS amps, round(watts, 2) AS watts
FROM pin_readings WHERE amps >= 9.2
ORDER BY unix_time_ms DESC LIMIT 100;

-- Sensor errors, alert episodes, protective actions, and app lifecycle events.
SELECT datetime(unix_time_ms / 1000.0, 'unixepoch') AS utc,
       type, action, details_json
FROM events ORDER BY unix_time_ms DESC LIMIT 50;
