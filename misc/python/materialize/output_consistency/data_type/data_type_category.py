# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from enum import Enum


class DataTypeCategory(Enum):
    """Data type category for the input parameters and return type of operation / function"""

    ANY = 10
    """Suitable for all type (e.g., `NULLIF`). Allowed as input parameters but not as return type"""
    DYNAMIC = 20
    """Dynamic type, only allowed as return type. The actual type will be resolved based on the first input arg"""

    NUMERIC = 101
    BOOLEAN = 102
    STRING = 103
    DATE_TIME = 104
    BYTEA = 105
    JSONB = 106
    UUID = 107

    ENUM = 200

    ARRAY = 300
    LIST = 301
    MAP = 302
    RANGE = 303

    RECORD = 400

    UNDETERMINED = 500
