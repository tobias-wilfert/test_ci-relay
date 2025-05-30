import json
from datetime import datetime, timedelta, timezone

from sentry_sdk.envelope import Envelope, Item, PayloadRef

from .asserts.time import time_within_delta


TEST_CONFIG = {
    "aggregator": {
        "bucket_interval": 1,
        "initial_delay": 0,
    }
}


def envelope_with_sentry_logs(*payloads: dict) -> Envelope:
    envelope = Envelope()
    envelope.add_item(
        Item(
            type="log",
            payload=PayloadRef(json={"items": payloads}),
            content_type="application/vnd.sentry.items.log+json",
            headers={"item_count": len(payloads)},
        )
    )
    return envelope


def envelope_with_otel_logs(start: datetime) -> Envelope:
    envelope = Envelope()
    envelope.add_item(
        Item(
            type="otel_log",
            payload=PayloadRef(
                bytes=json.dumps(
                    {
                        "timeUnixNano": str(int(start.timestamp() * 1e9)),
                        "observedTimeUnixNano": str(int(start.timestamp() * 1e9)),
                        "severityNumber": 10,
                        "severityText": "Information",
                        "traceId": "5B8EFFF798038103D269B633813FC60C",
                        "spanId": "EEE19B7EC3C1B174",
                        "body": {"stringValue": "Example log record"},
                        "attributes": [
                            {
                                "key": "string.attribute",
                                "value": {"stringValue": "some string"},
                            },
                            {"key": "boolean.attribute", "value": {"boolValue": True}},
                            {"key": "int.attribute", "value": {"intValue": "10"}},
                            {
                                "key": "double.attribute",
                                "value": {"doubleValue": 637.704},
                            },
                        ],
                    }
                ).encode()
            ),
        )
    )
    return envelope


def test_ourlog_extraction_with_otel_logs(
    mini_sentry,
    relay_with_processing,
    ourlogs_consumer,
):
    ourlogs_consumer = ourlogs_consumer()
    project_id = 42
    project_config = mini_sentry.add_full_project_config(project_id)
    project_config["config"]["features"] = [
        "organizations:ourlogs-ingestion",
    ]

    relay = relay_with_processing(options=TEST_CONFIG)

    start = datetime.now(timezone.utc)

    duration = timedelta(milliseconds=500)
    now = datetime.now(timezone.utc)
    end = now - timedelta(seconds=1)
    start = end - duration

    envelope = envelope_with_otel_logs(start)
    relay.send_envelope(project_id, envelope)

    ourlogs = ourlogs_consumer.get_ourlogs()
    expected = {
        "organization_id": 1,
        "project_id": 42,
        "retention_days": 90,
        "timestamp_nanos": int(start.timestamp() * 1e9),
        "observed_timestamp_nanos": time_within_delta(start, expect_resolution="ns"),
        "trace_id": "5b8efff798038103d269b633813fc60c",
        "body": "Example log record",
        "trace_flags": 0,
        "span_id": "eee19b7ec3c1b174",
        "severity_text": "Information",
        "severity_number": 10,
        "received": time_within_delta(start, expect_resolution="s"),
        "attributes": {
            "string.attribute": {"string_value": "some string"},
            "boolean.attribute": {"bool_value": True},
            "int.attribute": {"int_value": 10},
            "double.attribute": {"double_value": 637.704},
            "sentry.severity_number": {"int_value": 10},
            "sentry.severity_text": {"string_value": "Information"},
            "sentry.timestamp_nanos": {
                "string_value": str(int(start.timestamp() * 1e9))
            },
            "sentry.trace_flags": {"int_value": 0},
        },
    }

    del ourlogs[0]["attributes"]["sentry.observed_timestamp_nanos"]

    assert ourlogs == [expected]

    ourlogs_consumer.assert_empty()


def test_ourlog_extraction_with_sentry_logs(
    mini_sentry,
    relay_with_processing,
    ourlogs_consumer,
):
    ourlogs_consumer = ourlogs_consumer()
    project_id = 42
    project_config = mini_sentry.add_full_project_config(project_id)
    project_config["config"]["features"] = [
        "organizations:ourlogs-ingestion",
    ]

    relay = relay_with_processing(options=TEST_CONFIG)

    start = datetime.now(timezone.utc)

    envelope = envelope_with_sentry_logs(
        {
            "timestamp": start.timestamp(),
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "span_id": "eee19b7ec3c1b175",
            "level": "error",
            "body": "This is really bad",
        },
        {
            "timestamp": start.timestamp(),
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "span_id": "eee19b7ec3c1b174",
            "level": "info",
            "body": "Example log record",
            "severity_number": 10,
            "attributes": {
                "boolean.attribute": {"value": True, "type": "boolean"},
                "integer.attribute": {"value": 42, "type": "integer"},
                "double.attribute": {"value": 1.23, "type": "double"},
                "string.attribute": {"value": "some string", "type": "string"},
                "pii": {"value": "4242 4242 4242 4242", "type": "string"},
                "sentry.severity_text": {"value": "info", "type": "string"},
                "unknown_type": {"value": "info", "type": "unknown"},
                "broken_type": {"value": "info", "type": "not_a_real_type"},
                "mismatched_type": {"value": "some string", "type": "boolean"},
                "valid_string_with_other": {
                    "value": "test",
                    "type": "string",
                    "some_other_field": "some_other_value",
                },
            },
        },
    )
    relay.send_envelope(project_id, envelope)

    ourlogs = ourlogs_consumer.get_ourlogs()
    assert ourlogs == [
        {
            "organization_id": 1,
            "project_id": 42,
            "retention_days": 90,
            "timestamp_nanos": time_within_delta(start, expect_resolution="ns"),
            "observed_timestamp_nanos": time_within_delta(
                start, expect_resolution="ns"
            ),
            "received": time_within_delta(start, expect_resolution="s"),
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "span_id": "eee19b7ec3c1b175",
            "body": "This is really bad",
            "severity_text": "error",
            "severity_number": 17,
            "attributes": {
                "sentry.severity_number": {"int_value": 17},
                "sentry.severity_text": {"string_value": "error"},
            },
        },
        {
            "organization_id": 1,
            "project_id": 42,
            "retention_days": 90,
            "timestamp_nanos": time_within_delta(start, expect_resolution="ns"),
            "observed_timestamp_nanos": time_within_delta(
                start, expect_resolution="ns"
            ),
            "received": time_within_delta(start, expect_resolution="s"),
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "body": "Example log record",
            "span_id": "eee19b7ec3c1b174",
            "severity_text": "info",
            "severity_number": 9,
            "attributes": {
                "boolean.attribute": {"bool_value": True},
                "double.attribute": {"double_value": 1.23},
                "integer.attribute": {"int_value": 42},
                "string.attribute": {"string_value": "some string"},
                "valid_string_with_other": {"string_value": "test"},
                "pii": {"string_value": "[creditcard]"},
                "sentry.severity_number": {"int_value": 9},
                "sentry.severity_text": {"string_value": "info"},
            },
        },
    ]
    ourlogs_consumer.assert_empty()


def test_ourlog_extraction_with_sentry_logs_with_missing_fields(
    mini_sentry,
    relay_with_processing,
    ourlogs_consumer,
):

    ourlogs_consumer = ourlogs_consumer()
    project_id = 42
    project_config = mini_sentry.add_full_project_config(project_id)
    project_config["config"]["features"] = [
        "organizations:ourlogs-ingestion",
    ]

    relay = relay_with_processing(options=TEST_CONFIG)

    start = datetime.now(timezone.utc)

    envelope = envelope_with_sentry_logs(
        {
            "timestamp": start.timestamp(),
            "trace_id": "5b8efff798038103d269b633813fc60c",
            "level": "warn",
            "body": "Example log record 2",
        }
    )
    relay.send_envelope(project_id, envelope)

    ourlogs = ourlogs_consumer.get_ourlogs()
    expected = {
        "organization_id": 1,
        "project_id": 42,
        "retention_days": 90,
        "timestamp_nanos": time_within_delta(start, expect_resolution="ns"),
        "observed_timestamp_nanos": time_within_delta(start, expect_resolution="ns"),
        "received": time_within_delta(start, expect_resolution="s"),
        "trace_id": "5b8efff798038103d269b633813fc60c",
        "body": "Example log record 2",
        "severity_text": "warn",
        "severity_number": 13,
        "attributes": {
            "sentry.severity_number": {"int_value": 13},
            "sentry.severity_text": {"string_value": "warn"},
        },
    }
    assert ourlogs == [expected]
    ourlogs_consumer.assert_empty()


def test_ourlog_extraction_is_disabled_without_feature(
    mini_sentry,
    relay_with_processing,
    ourlogs_consumer,
):
    ourlogs_consumer = ourlogs_consumer()
    relay = relay_with_processing(options=TEST_CONFIG)
    project_id = 42
    project_config = mini_sentry.add_full_project_config(project_id)
    project_config["config"]["features"] = []

    start = datetime.now(timezone.utc)

    envelope = envelope_with_otel_logs(start)
    relay.send_envelope(project_id, envelope)

    ourlogs = ourlogs_consumer.get_ourlogs()

    assert len(ourlogs) == 0
    ourlogs_consumer.assert_empty()
