import json
import math
import queue
import socket
import time
import threading

import pytest

from flask import request as flask_request
from requests.exceptions import HTTPError
import zstandard


def test_local_project_config(mini_sentry, relay):
    project_id = 42
    config = mini_sentry.basic_project_config(project_id)
    relay_config = {
        "cache": {
            "file_interval": 1,
            "project_expiry": 1,
            "project_grace_period": 0,
        }
    }
    relay = relay(mini_sentry, relay_config, wait_health_check=False)
    relay.config_dir.mkdir("projects").join("42.json").write(
        json.dumps(
            {
                # remove defaults to assert they work
                "publicKeys": config["publicKeys"],
                "config": {
                    "allowedDomains": ["*"],
                    "trustedRelays": [],
                    "piiConfig": {},
                },
            }
        )
    )
    # get the dsn key from the config
    # we need to provide it manually to Relay since it is not in the config (of MiniSentry) and
    # we don't look on the file system
    dsn_key = config["publicKeys"][0]["publicKey"]

    relay.wait_relay_health_check()
    mini_sentry.clear_test_failures()

    relay.send_event(project_id, dsn_key=dsn_key)
    event = mini_sentry.captured_events.get(timeout=1).get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}

    relay.config_dir.join("projects").join("42.json").write(
        json.dumps({"disabled": True, "publicKeys": config["publicKeys"]})
    )

    time.sleep(2)

    relay.send_event(project_id, dsn_key=dsn_key)
    time.sleep(0.3)
    pytest.raises(HTTPError, lambda: relay.send_event(project_id, dsn_key=dsn_key))
    pytest.raises(queue.Empty, lambda: mini_sentry.captured_events.get(timeout=1))


@pytest.mark.parametrize("grace_period", [0, 5])
def test_project_grace_period(mini_sentry, relay, grace_period):
    config = mini_sentry.add_basic_project_config(42)
    config["disabled"] = True
    fetched_project_config = threading.Event()

    get_project_config_original = mini_sentry.app.view_functions["get_project_config"]

    @mini_sentry.app.endpoint("get_project_config")
    def get_project_config():
        if not flask_request.json.get("global") is True:
            fetched_project_config.set()
        return get_project_config_original()

    relay = relay(
        mini_sentry,
        {
            "cache": {
                "miss_expiry": 1,
                "project_expiry": 1,
                "project_grace_period": grace_period,
            }
        },
    )

    assert not fetched_project_config.is_set()

    # The first event sent should always return a 200 because we have no
    # project config
    relay.send_event(42)

    assert fetched_project_config.wait(timeout=1)
    time.sleep(2)
    fetched_project_config.clear()

    if grace_period == 0:
        # With a grace period of 0, sending a second event should return a 200
        # because the project config has expired again and needs to be refetched
        relay.send_event(42)
    else:
        assert not fetched_project_config.is_set()

        # With a non-zero grace period the request should fail during the grace period
        with pytest.raises(HTTPError) as excinfo:
            relay.send_event(42)

        assert excinfo.value.response.status_code == 403
        assert fetched_project_config.wait(timeout=1)

    assert mini_sentry.captured_events.empty()


@pytest.mark.parametrize("failure_type", ["timeout", "socketerror"])
def test_query_retry(failure_type, mini_sentry, relay):
    retry_count = 0

    mini_sentry.add_basic_project_config(42)
    original_endpoint = mini_sentry.app.view_functions["get_project_config"]

    @mini_sentry.app.endpoint("get_project_config")
    def get_project_config():
        if flask_request.json.get("global") is True:
            return original_endpoint()

        nonlocal retry_count
        retry_count += 1
        print("RETRY", retry_count)

        if retry_count < 3:
            if failure_type == "timeout":
                time.sleep(50)  # ensure timeout
            elif failure_type == "socketerror":
                raise OSError()
            else:
                assert False

            return "ok"  # never read by client
        else:
            return original_endpoint()

    relay = relay(mini_sentry)

    try:
        relay.send_event(42)

        event = mini_sentry.captured_events.get(timeout=12).get_event()
        assert event["logentry"] == {"formatted": "Hello, World!"}
        assert retry_count == 3

        for _, error in mini_sentry.current_test_failures():
            assert isinstance(error, (socket.error, AssertionError))
    finally:
        mini_sentry.clear_test_failures()


def test_query_retry_maxed_out(mini_sentry, relay):
    """
    Assert that a query is not retried an infinite amount of times.
    """
    request_count = 0

    original_get_project_config = mini_sentry.app.view_functions["get_project_config"]

    @mini_sentry.app.endpoint("get_project_config")
    def get_project_config():
        if flask_request.json.get("global") is True:
            return original_get_project_config()
        nonlocal request_count
        request_count += 1
        print("RETRY", request_count)
        return "no", 500

    RETRIES = 1
    query_timeout = 0.5  # Initial grace period
    # Relay's exponential backoff: INITIAL_INTERVAL = 1s; DEFAULT_MULTIPLIER = 1.5;
    for retry in range(RETRIES):  # 1 retry
        query_timeout += 1 * 1.5 ** (retry + 1)

    relay = relay(mini_sentry, {"limits": {"query_timeout": math.ceil(query_timeout)}})

    # No error messages yet
    assert mini_sentry.test_failures.empty()

    try:
        relay.send_event(42)
        time.sleep(query_timeout)
        assert request_count == 1 + RETRIES
        assert {str(e) for _, e in mini_sentry.current_test_failures()} == {
            "Relay sent us event: error fetching project states: upstream request returned error 500 Internal Server Error: no error details",
        }

        time.sleep(1)  # Wait for project to be cached

        # Relay still accepts events for this project
        next_response = relay.send_event(42)
        assert "id" in next_response
    finally:
        mini_sentry.clear_test_failures()


@pytest.mark.parametrize("disabled", (True, False))
def test_processing_redis_query(
    mini_sentry,
    redis_client,
    relay_with_processing,
    events_consumer,
    outcomes_consumer,
    disabled,
):
    outcomes_consumer = outcomes_consumer()
    events_consumer = events_consumer()

    relay = relay_with_processing({"limits": {"query_timeout": 10}})
    project_id = 42
    cfg = mini_sentry.add_full_project_config(project_id)
    cfg["disabled"] = disabled

    key = mini_sentry.get_dsn_public_key(project_id)
    projectconfig_cache_prefix = relay.options["processing"][
        "projectconfig_cache_prefix"
    ]
    redis_client.setex(f"{projectconfig_cache_prefix}:{key}", 3600, json.dumps(cfg))

    relay.send_event(project_id)

    if disabled:
        outcome = outcomes_consumer.get_outcome()
        assert (outcome["outcome"], outcome["reason"]) == (3, "project_id")
    else:
        event, v = events_consumer.get_event()
        assert event["logentry"] == {"formatted": "Hello, World!"}


def test_processing_redis_query_compressed(
    mini_sentry, redis_client, relay_with_processing, events_consumer, outcomes_consumer
):
    outcomes_consumer = outcomes_consumer()
    events_consumer = events_consumer()

    relay = relay_with_processing({"limits": {"query_timeout": 10}})
    project_id = 42
    cfg = mini_sentry.add_full_project_config(project_id)

    key = mini_sentry.get_dsn_public_key(project_id)
    projectconfig_cache_prefix = relay.options["processing"][
        "projectconfig_cache_prefix"
    ]
    redis_client.setex(
        f"{projectconfig_cache_prefix}:{key}",
        3600,
        zstandard.compress(json.dumps(cfg).encode()),
    )

    relay.send_event(project_id)

    event, _ = events_consumer.get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}


def test_processing_redis_query_with_revision(
    mini_sentry,
    redis_client,
    relay_with_processing,
    events_consumer,
    outcomes_consumer,
):
    outcomes_consumer = outcomes_consumer()
    events_consumer = events_consumer()

    relay = relay_with_processing(
        {"limits": {"query_timeout": 10}, "cache": {"project_expiry": 1}}
    )
    project_id = 42
    cfg = mini_sentry.add_full_project_config(project_id)
    cfg["rev"] = "123"

    key = mini_sentry.get_dsn_public_key(project_id)
    projectconfig_cache_prefix = relay.options["processing"][
        "projectconfig_cache_prefix"
    ]
    redis_client.setex(f"{projectconfig_cache_prefix}:{key}", 3600, json.dumps(cfg))
    redis_client.setex(f"{projectconfig_cache_prefix}:{key}.rev", 3600, cfg["rev"])

    relay.send_event(project_id)
    event, _ = events_consumer.get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}

    # 1 second timeout on the project cache, make sure it times out
    time.sleep(2)

    relay.send_event(project_id)
    event, _ = events_consumer.get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}


@pytest.mark.parametrize("with_revision_support", [True, False])
def test_project_fetch_revision(mini_sentry, relay, with_revision_support):
    project_config_fetch = threading.Event()
    with_rev = threading.Event()

    get_project_config_original = mini_sentry.app.view_functions["get_project_config"]

    @mini_sentry.app.endpoint("get_project_config")
    def get_project_config():
        if not flask_request.json.get("global"):
            project_config_fetch.set()
        if flask_request.json.get("revisions") == ["123"]:
            with_rev.set()

        return get_project_config_original()

    mini_sentry.project_config_ignore_revision = not with_revision_support
    config = mini_sentry.add_basic_project_config(42)
    config["rev"] = "123"

    relay = relay(
        mini_sentry,
        {
            "cache": {
                "miss_expiry": 1,
                "project_expiry": 1,
                "project_grace_period": 5,
            }
        },
    )

    relay.send_event(42)

    assert project_config_fetch.wait(timeout=1)
    project_config_fetch.clear()
    assert not with_rev.is_set()

    event = mini_sentry.captured_events.get(timeout=1).get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}

    # Wait for project config to expire.
    time.sleep(2)

    relay.send_event(42)

    assert project_config_fetch.wait(timeout=2)
    assert with_rev.wait(timeout=2)

    event = mini_sentry.captured_events.get(timeout=1).get_event()
    assert event["logentry"] == {"formatted": "Hello, World!"}
