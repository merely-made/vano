# Notes on test262 expectations

`expectations.json` is a plain path-to-result map and cannot carry comments, so
known deviations are recorded here.

- `staging/sm/RegExp/unicode-ignoreCase-escape.js` (FAIL): `/[^\W]/iu` does not
  match "S", "s" or U+017F. Introduced by f16a746b (RegExp matching moved from
  the `regex` crate to `regress` 0.11.1); a bug in `regress`, not in this
  repository.
- `language/statements/class/elements/privatefieldset-evaluation-order-1.js`
  (FAIL): destructuring assignment to `this.x` evaluates the right-hand side
  before resolving `this`, so a `super()` call inside a getter initializes `this`
  too early. The test used to pass by accident, because derived-class field
  initializers ran before `super()` and threw the expected ReferenceError (#948).
  Exposed by the derived-class field fix cherry-picked from trynova/nova#992.
- `built-ins/Object/defineProperty/15.2.3.6-4-116.js` (FAIL, TIMEOUT on slow
  machines): `verifyProperty` sets an array's `length` to 2**32-1 and back, and
  shrinking walks every index. Already slow at bece61ac.
