---
title: "DROP INDEX"
description: "DROP INDEX removes an index"
menu:
  main:
    parent: 'commands'
---

`DROP INDEX` removes an index from a materialized view. (Non-materialized views do not have any indexes.)

## Syntax

{{< diagram "drop-index.svg" >}}

Field | Use
------|-----
**IF EXISTS** | Do not return an error if the named index doesn't exist.
_index&lowbar;name_ | The name of the index you want to remove.
**CASCADE** | Remove the index and its dependent objects.
**RESTRICT** |  Remove the index. _(Default.)_

**Note:** Since indexes cannot currently have dependent objects, `DROP INDEX`, `DROP INDEX RESTRICT`, and `DROP INDEX CASCADE` all do the same thing.

## Examples

### Remove an index

```mzsql
SHOW VIEWS;
```
```nofmt
+-----------------------------------+
| VIEWS                             |
|-----------------------------------|
| ...                               |
| q01                               |
+-----------------------------------+
```
```mzsql
SHOW INDEXES ON q01;
```
```nofmt
+--------------------------------+------------------------+-----
| name                           | on                     | ...
|--------------------------------+------------------------+----
| materialize.public.q01_geo_idx | materialize.public.q01 | ...
+--------------------------------+------------------------+-----
```

You can use the unqualified index name (`q01_geo_idx`) rather the fully qualified name (`materialize.public.q01_geo_idx`).

You can remove an index with any of the following commands:

- ```mzsql
  DROP INDEX q01_geo_idx;
  ```
- ```mzsql
  DROP INDEX q01_geo_idx RESTRICT;
  ```
- ```mzsql
  DROP INDEX q01_geo_idx CASCADE;
  ```

### Do not issue an error if attempting to remove a nonexistent index

```mzsql
DROP INDEX IF EXISTS q01_geo_idx;
```

## Privileges

The privileges required to execute this statement are:

- Ownership of the dropped index.
- `USAGE` privileges on the containing schema.

## Related pages

- [`SHOW VIEWS`](../show-views)
- [`SHOW INDEXES`](../show-indexes)
- [DROP OWNED](../drop-owned)
