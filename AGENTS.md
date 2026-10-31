# Agent completion

1. Add or update regression tests for changed behavior. During iteration, run
   `pnpm verify:changed` (README "Verification" lists what it runs per path).
2. Before handing back code changes, run `pnpm verify` from the repository root.
   Fix failures caused by the change; report pre-existing failures separately.
3. Exit code 3 with a `SETUP` line means dependencies are missing: run the printed
   command and retry. It is not a test result.
4. Never dismiss a failing test as flaky. Record its name and output. You may rerun
   that one test once to check reproducibility, but a passing rerun does not make
   the gate pass: report the original failure.
5. Report the verification command and result, plus any behavior left unverified
   and why. A passing local gate does not establish hardware or provider behavior.

For UI changes, verify the affected view in a real browser or the running app.
For capture, hotkey, destination or insertion changes, read `bench/README.md` for
desktop checks. These require a Wayland session and can send input to other apps;
arrange a dedicated run rather than running them during normal desktop use.

For cleanup prompt changes or live latency comparisons, read `bench/README.md`
first. Its cleanup runner targets a historical provider, not production Qwen.
Live checks use credentials and may incur charges; keep them explicitly opt-in.
