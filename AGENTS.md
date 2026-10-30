# Agent completion

1. Add or update regression tests for changed behavior. During iteration, run the
   narrowest affected check from the README's "Verification" section.
2. Before handing back code changes, run `pnpm verify` from the repository root.
   Fix failures caused by the change; report pre-existing failures separately.
3. Report the verification command and result, plus any behavior left unverified
   and why. A passing local gate does not establish hardware or provider behavior.

For UI changes, verify the affected view in a real browser or the running app.
For capture, hotkey, destination or insertion changes, read `bench/README.md` for
desktop checks. These require a Wayland session and can send input to other apps;
arrange a dedicated run rather than running them during normal desktop use.

For cleanup prompt changes or live latency comparisons, read `bench/README.md`
first. Its cleanup runner targets a historical provider, not production Qwen.
Live checks use credentials and may incur charges; keep them explicitly opt-in.
