# tisqlx documentation

Four kinds of pages, for four kinds of needs:

| | when you are learning | when you are working |
|---|---|---|
| **doing** | **[Tutorial](tutorial.md)**: build a first program, step by step, from an empty directory to checked queries and migrations. | **[How-to guides](how-to.md)**: one recipe per task: retry a transaction, load many rows, test against a database, build in CI, serve HTTP. |
| **understanding** | **[Explanation](explanation.md)**: why tisqlx works the way it does, and what TiDB does differently from MySQL. | **[Reference](reference.md)**: every URL key, pool option, macro rule, type mapping, error and CLI command. |

A complete program to read or copy: [`tisqlx/examples/axum.rs`](../tisqlx/examples/axum.rs), an
HTTP API in one file ([how to run it](how-to.md#serve-an-http-api-with-axum)).

For working on tisqlx itself: [TESTING.md](../TESTING.md) and [TODO.md](../TODO.md).
