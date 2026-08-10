# Canonical digest bytes

Canonforge hashes exact UTF-8 bytes. SHA-256 values are lowercase hexadecimal.

## Span text

`text_sha256` covers the span's literal UTF-8 text bytes with no normalization,
prefix, suffix, or trailing newline added by Canonforge.

## Evidence unit

For schemas v2 and v3, `unit_sha256` covers compact JSON for these fields in
this exact order and excludes `unit_sha256` itself:

1. `schema_version`
2. `unit_id`
3. `source_type`
4. `source_locator`
5. `metadata`
6. `sources`
7. `spans`
8. `attachments`

Schema v1 uses the same order but ends after `spans`; it has no `attachments`
field.

There is no whitespace or trailing newline. Encoding is UTF-8 and uses these
rules:

- The eight top-level fields use the order above.
- Every object inside `source_locator` and `metadata` sorts keys by Unicode
  scalar value at every level. Arrays retain source order.
- Each `sources` object uses `path`, `sha256`, `bytes` order.
- Each `spans` object uses `id`, `locator`, `role`, `timestamp`,
  `text_sha256`, `text` order.
- Each schema-v2 `attachments` object uses `id`, `span_id`, `locator`,
  `filename`, `media_type`, `disposition`, `content_id`, `source` order. Its
  `source` object uses the same order as a unit source receipt.
- Each schema-v3 `attachments` object uses `id`, `span_id`, `locator`,
  `filename`, `media_type`, `disposition`, `content_id`, followed by exactly
  one of `source` or `error`. A materialized `source` uses the same order as a
  unit source receipt. The only unavailable `error` value is
  `malformed-or-undecodable-transfer`.
- Strings escape quotation mark and reverse solidus as `\"` and `\\`. They use
  the short escapes `\b`, `\t`, `\n`, `\f`, and `\r`; other U+0000 through
  U+001F values use lowercase `\u00xx`. Other Unicode scalar values are literal
  UTF-8. Canonforge performs no Unicode normalization and does not escape `/`.
- Integers use the shortest base-10 representation. Inputs may spell a
  mathematical integer as an integral JSON number such as `1.0`; Canonforge
  canonicalizes it to `1`. Values below `-9007199254740991`, above
  `9007199254740991`, or with a fractional part are outside the contract. This
  is the IEEE-754 exact-integer range, so conforming JSON implementations can
  preserve the value without rounding. Booleans and null are the lowercase
  JSON literals.

This conformance vector exercises non-ASCII text, LF, U+0001, nested metadata,
and every nested field order:

```json
{"schema_version":3,"unit_id":"unit:é","source_type":"canonical-markdown","source_locator":{"file":"notes/é.md","line":7},"metadata":{"count":2,"nested":{"enabled":true,"labels":["α","line\nbreak"]}},"sources":[{"path":"notes/é.md","sha256":"0000000000000000000000000000000000000000000000000000000000000000","bytes":12}],"spans":[{"id":"unit:é#span=1","locator":"notes/é.md#line=7","role":"heading","timestamp":null,"text_sha256":"1111111111111111111111111111111111111111111111111111111111111111","text":"Café\n\u0001"}],"attachments":[]}
```

SHA-256:
`a3cefec463d9c9e84e22149cff7daac493f4819e1c2019c1a0948663e997097c`
