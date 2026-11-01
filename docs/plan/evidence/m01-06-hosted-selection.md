# Hosted selection qualification fixture

This temporary draft PR exercises the reviewed M01.06 workflow against its implementation branch. This first commit changes only an allowlisted narrative evidence file. The expected plan selects `docs`, with native and sandbox jobs explicitly skipped and `ci-ok` requiring successful planning and docs checks.

Subsequent fixture commits will exercise types-only and engine-only Rust selections independently. Actual heads, plans and hosted check-run results will be preserved in the delivery evidence. This fixture is not a product implementation or a passing-result claim, and is not intended for integration into main.
