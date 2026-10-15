# Recorded child reports after compaction

Wake mode keeps a terminal child report eligible to trigger a parent turn until the parent accepts it from model-visible history. If compaction replaces that history without retaining the report's response-item ID, the report can remain recorded but disappear from the prompt. The parent then cannot accept it and can repeatedly wake on the same report.

After a successful history replacement, Codex now returns omitted recorded trigger reports to normal mailbox delivery. Delivery restores them to model-visible history, where the parent can accept them. Reports retained by compaction remain recorded, and queue-only reports do not trigger automatic turns. See [issue #175](https://github.com/epsalmond/codex/issues/175), related to [wake mode #19](https://github.com/epsalmond/codex/issues/19).
