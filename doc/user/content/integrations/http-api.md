---
title: "Connect to Materialize via HTTP"
description: "How to use Materialize via HTTP"
menu:
  main:
    parent: "integrations"
    weight: 50
    name: "HTTP API"
---

You can access Materialize through its "session-less" HTTP API endpoint:

```bash
https://<MZ host address>/api/sql
```

## Details

### General semantics

The API:

- Requires username/password authentication, just as connecting via a SQL
  client (e.g. `psql`). Materialize provides you the username and password upon
  setting up your account.
- Requires that you provide the entirety of your request. The API does not
  provide session-like semantics, so there is no way to e.g. interactively use
  transactions.
- Ceases to process requests upon encountering the first error.
- Does not support statements whose semantics rely on sessions or whose state is
  indeterminate at the end of processing, including:
    - `CLOSE`
    - `COPY`
    - `DECLARE`
    - `FETCH`
    - `SUBSCRIBE`
- Supports specifying run-time [configuration parameters](/sql/set)
  via URL query parameters.

### Transactional semantics

The HTTP API provides two modes with slightly different transactional semantics from one another:

- **Simple**, which mirrors PostgreSQL's [Simple Query][simple-query] protocol.
    - Supports a single query, but the single query string may contain multiple
      statements, e.g. `SELECT 1; SELECT 2;`
    - Treats all statements as in an implicit transaction unless other
      transaction control is invoked.
- **Extended**, which mirrors PostgreSQL's [Extended Query][extended-query] protocol.
    - Supports multiple queries, but only one statement per query string.
    - Supports parameters.
    - Eagerly commits DDL (e.g. `CREATE TABLE`) in implicit transactions, but
      not DML (e.g. `INSERT`).

### OpenAPI spec

Download our [OpenAPI](https://swagger.io/specification/) v3 spec for this
interface: [environmentd-openapi.yml](/materialize-openapi.yml).

## Usage

### Endpoint

```
https://<MZ host address>/api/sql
```

Accessing the endpoint requires [basic authentication](https://developer.mozilla.org/en-US/docs/Web/HTTP/Authentication#basic_authentication_scheme). Reuse the same credentials as with a SQL client (e.g. `psql`):

* **User ID:** Your email to access Materialize.
* **Password:** Your app password.

#### Query parameters

You can optionally specify configuration parameters for each request, as a URL-encoded JSON object, with the `options` query parameter:

```
https://<MZ host address>/api/sql?options=<object>
```

For example, this is how you could specify the `application_name` configuration parameter with JavaScript:

```javascript
// Create and encode our parameters object.
const options = { application_name: "my_app" };
const encoded = encodeURIComponent(JSON.stringify(options));

// Add the object to our URL as the "options" query parameter.
const url = new URL(`https://${mzHostAddress}/api/sql`);
url.searchParams.append("options", encoded);
```

### Input format

#### Simple

The request body is a JSON object containing a key, `query`, which specifies the
SQL string to execute. `query` may contain multiple SQL statements separated by
semicolons.

```json
{
    "query": "select * from a; select * from b;"
}
```

#### Extended

The request body is a JSON object containing a key `queries`, whose value is
array of objects, whose structure is:

Key | Value
----|------
`query` | A SQL string containing one statement to execute
`params` | An optional array of text values to be used as the parameters to `query`. _null_ values are converted to _null_ values in Materialize. Note that all parameter values' elements must be text or _null_; the API will not accept JSON numbers.

```json
{
    "queries": [
        { "query": "select * from a;" },
        { "query": "select a + $1 from a;", "params": ["100"] }
        { "query": "select a + $1 from a;", "params": [null] }
    ]
}
```

### Output format

The output format is a JSON object with one key, `results`, whose value is
an array of the following:

Result | JSON value
---------------------|------------
Rows | `{"rows": <2D array of JSON-ified results>, "desc": <array of column descriptions>, "notices": <array of notices>}`
Error | `{"error": <Error object from execution>, "notices": <array of notices>}`
Ok | `{"ok": <tag>, "notices": <array of notices>}`

Each committed statement returns exactly one of these values; e.g. in the case
of "complex responses", such as `INSERT INTO...RETURNING`, the presence of a
`"rows"` object implies `"ok"`.

The `"notices"` array is present in all types of results and contains any
diagnostic messages that were generated during execution of the query. It has
the following structure:

```
{"severity": <"warning"|"notice"|"debug"|"info"|"log">, "message": <informational message>}
```

Note that the returned values include the results of statements which were
ultimately rolled back because of an error in a later part of the transaction.
You must parse the results to understand which statements ultimately reflect
the resultant state.

Numeric results are converted to strings to avoid possible JavaScript number inaccuracy.
Column descriptions contain the name, oid, data type size and type modifier of a returned column.

#### TypeScript definition

You can model these with the following TypeScript definitions:

```typescript
interface Simple {
    query: string;
}

interface ExtendedRequest {
    query: string;
    params?: (string | null)[];
}

interface Extended {
    queries: ExtendedRequest[];
}

type SqlRequest = Simple | Extended;

interface Notice {
	message: string;
	severity: string;
	detail?: string;
	hint?: string;
}

interface Error {
	message: string;
	code: string;
	detail?: string;
	hint?: string;
}

interface Column {
    name: string;
    type_oid: number; // u32
    type_len: number; // i16
    type_mod: number; // i32
}

interface Description {
	columns: Column[];
}

type SqlResult =
  | {
	tag: string;
	rows: any[][];
	desc: Description;
	notices: Notice[];
} | {
	ok: string;
	notices: Notice[];
} | {
	error: Error;
	notices: Notice[];
};
```

## Examples
### Run a transaction

Use the [extended input format](#extended) to run a transaction:
```bash
curl 'https://<MZ host address>/api/sql' \
    --header 'Content-Type: application/json' \
    --user '<username>:<passsword>' \
    --data '{
        "queries": [
            { "query": "CREATE TABLE IF NOT EXISTS t (a int);" },
            { "query": "CREATE TABLE IF NOT EXISTS s (a int);" },
            { "query": "BEGIN;" },
            { "query": "INSERT INTO t VALUES ($1), ($2)", "params": ["100", "200"] },
            { "query": "COMMIT;" },
            { "query": "BEGIN;" },
            { "query": "INSERT INTO s VALUES ($1), ($2)", "params": ["9", null] },
            { "query": "COMMIT;" }
        ]
    }'
```

Response:
```json
{
  "results": [
    {"ok": "CREATE TABLE", "notices": []},
    {"ok": "CREATE TABLE", "notices": []},
    {"ok": "BEGIN", "notices": []},
    {"ok": "INSERT 0 2", "notices": []},
    {"ok": "COMMIT", "notices": []},
    {"ok": "BEGIN", "notices": []},
    {"ok": "INSERT 0 2", "notices": []},
    {"ok": "COMMIT", "notices": []}
  ]
}
```

### Run a query

Use the [simple input format](#simple) to run a query:
```bash
curl 'https://<MZ host address>/api/sql' \
    --header 'Content-Type: application/json' \
    --user '<username>:<passsword>' \
    --data '{
        "query": "SELECT t.a + s.a AS cross_add FROM t CROSS JOIN s; SELECT a FROM t WHERE a IS NOT NULL;"
    }'
```

Response:
```json
{
  "results": [
    {
      "desc": {
        "columns": [
          {
            "name": "cross_add",
            "type_len": 4,
            "type_mod": -1,
            "type_oid": 23
          }
        ]
      },
      "notices": [],
      "rows": [],
      "tag": "SELECT 0"
    },
    {
      "desc": {
        "columns": [
          {
            "name": "a",
            "type_len": 4,
            "type_mod": -1,
            "type_oid": 23
          }
        ]
      },
      "notices": [],
      "rows": [],
      "tag": "SELECT 0"
    }
  ]
}
```

## See also
- [SQL Clients](../sql-clients)

[simple-query]: https://www.postgresql.org/docs/current/protocol-flow.html#id-1.10.5.7.4
[extended-query]: https://www.postgresql.org/docs/current/protocol-flow.html#PROTOCOL-FLOW-EXT-QUERY
