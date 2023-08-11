# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from typing import Any, Dict, Optional

import requests
from requests import Response

from ..config.environment_config import EnvironmentConfig
from .common import retry
from .controller import wait_for_connectable
from .http import delete, get, patch
from .kubectl import (
    kubectl_get,
    kubectl_get_or_none,
    kubectl_get_retry,
)


def create_environment_assignment(
    config: EnvironmentConfig,
    image: Optional[str] = None,
) -> Dict[str, Any]:
    environment_assignment = f"{config.auth.organization_id}-0"
    environment = f"environment-{environment_assignment}"

    json: dict[str, Any] = {}
    if image is not None:
        json["environmentdImageRef"] = image
    patch(
        config,
        config.controllers.region_api_server.configured_base_url(),
        "/api/region",
        json,
    )
    kubectl_get_retry(
        config.environment_context,
        None,
        "environment",
        environment,
        10,
    )
    return kubectl_get(
        config.system_context,
        None,
        "environmentassignment",
        environment_assignment,
    )


def wait_for_environmentd(config: EnvironmentConfig) -> Dict[str, Any]:
    def get_environment() -> Response:
        response = get(
            config,
            config.controllers.region_api_server.configured_base_url(),
            "/api/region",
        )
        region_info = response.json().get("regionInfo")
        assert region_info
        assert region_info.get("resolvable")
        assert region_info.get("sqlAddress")
        return response

    environment_json = retry(get_environment, 600, [AssertionError]).json()
    pgwire_url = environment_json["regionInfo"]["sqlAddress"]
    (pgwire_host, pgwire_port) = pgwire_url.split(":")
    wait_for_connectable((pgwire_host, int(pgwire_port)), 300)
    return environment_json


def delete_environment_assignment(config: EnvironmentConfig) -> None:
    environment_assignment = f"{config.auth.organization_id}-0"
    environment = f"environment-{environment_assignment}"

    def delete_environment():
        delete(
            config,
            config.controllers.region_api_server.configured_base_url(),
            "/api/region",
            # we have a 60 second timeout in the region api's load balancer
            # for this call and a 5 minute timeout in the region api (which
            # is relevant when running in kind)
            timeout=305,
        )

    retry(delete_environment, 10, [requests.exceptions.HTTPError])

    assert (
        kubectl_get_or_none(
            context=config.environment_context,
            namespace=None,
            resource_type="namespace",
            resource_name=environment,
        )
        is None
    )
    assert (
        kubectl_get_or_none(
            context=config.environment_context,
            namespace=None,
            resource_type="environment",
            resource_name=environment,
        )
        is None
    )
    assert (
        kubectl_get_or_none(
            context=config.system_context,
            namespace=None,
            resource_type="environmentassignment",
            resource_name=environment_assignment,
        )
        is None
    )
