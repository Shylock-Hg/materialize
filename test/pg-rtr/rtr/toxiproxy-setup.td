# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ http-request method=POST url=http://toxiproxy:8474/proxies content-type=application/json
{
  "name": "pg_1",
  "listen": "0.0.0.0:5432",
  "upstream": "postgres:5432"
}

$ http-request method=POST url=http://toxiproxy:8474/proxies content-type=application/json
{
  "name": "pg_2",
  "listen": "0.0.0.0:4432",
  "upstream": "postgres:5432"
}

# Run each source at a different impedance

$ http-request method=POST url=http://toxiproxy:8474/proxies/pg_1/toxics content-type=application/json
{
  "name": "pg_1",
  "type": "bandwidth",
  "attributes": { "rate": 10 }
}

$ http-request method=POST url=http://toxiproxy:8474/proxies/pg_2/toxics content-type=application/json
{
  "name": "pg_2",
  "type": "bandwidth",
  "attributes": { "rate": 30 }
}
