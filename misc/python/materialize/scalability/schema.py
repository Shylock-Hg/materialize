# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from enum import Enum, auto
from typing import Optional


class Source(Enum):
    TABLE = auto()


class TransactionIsolation(Enum):
    SERIALIZABLE = "serializable"
    STRICT_SERIALIZABLE = "strict serializable"

    def __str__(self) -> str:
        return self.value


class Schema:
    def __init__(
        self,
        source: Source = Source.TABLE,
        schema: str = "scalability",
        create_index: bool = True,
        transaction_isolation: Optional[TransactionIsolation] = None,
        cluster_name: Optional[str] = None,
    ) -> None:
        self.schema = schema
        self.source = source
        self.create_index = create_index
        self.transaction_isolation = transaction_isolation
        self.cluster_name = cluster_name

    def init_sqls(self) -> list[str]:
        init_sqls = self.connect_sqls() + [
            f"DROP SCHEMA IF EXISTS {self.schema} CASCADE;",
            f"CREATE SCHEMA {self.schema};",
            "DROP TABLE IF EXISTS t1;",
        ]
        if self.source == Source.TABLE:
            init_sqls.extend(
                [
                    "CREATE TABLE t1 (f1 INTEGER DEFAULT 1);",
                    "INSERT INTO t1 DEFAULT VALUES;",
                ]
            )

        if self.create_index:
            init_sqls.append("CREATE INDEX i1 ON t1 (f1);")

        return init_sqls

    def connect_sqls(self) -> list[str]:
        init_sqls = [f"SET SCHEMA = {self.schema};"]
        if self.cluster_name is not None:
            init_sqls.append(f"SET cluster_name = {self.cluster_name};")

        if self.transaction_isolation is not None:
            init_sqls.append(
                f"SET transaction_isolation = '{self.transaction_isolation}';"
            )

        return init_sqls