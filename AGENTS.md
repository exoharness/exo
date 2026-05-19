# General guidance

- Do not write fallback code or handle backwards compatability unless a feature involves state that is already committed on the main branch (not your feature branch). We generally don't need to worry about back-compat, especially for features that are inflight.
- Do not parse `Value` objects directly. Serialize/deserialize into structs.
- Do not write custom serializers/deserializers without explicit permission. It's rare that you need to do this. In general, you should use serde tags and keep data structures simple / literal.
- In rust, do not do `let _ = ...` unless the RHS returns a value you are explicitly ignoring (like an error, which btw, is bad).
