"""Deterministic cleanup evaluator: no LLM judge, no network. See README "Cleanup cases".

`evaluate(case, output)` returns hard failures (meaning changed: the output is unusable) and
soft failures (cleanup incomplete or cosmetic). Span checks run on normalized text, so
punctuation, case, "don't"/"do not" and "three"/"3" never decide a semantic test; `literal`
spans are the exception and match exactly.
"""
import re

NUMBERS = {w: str(i) for i, w in enumerate(
    "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen "
    "fifteen sixteen seventeen eighteen nineteen twenty".split())}
NUMBERS.update({w: str(30 + 10 * i) for i, w in enumerate("thirty forty fifty sixty seventy eighty ninety".split())})
NUMBERS.update({w: f"{i}{'st' if i == 1 else 'nd' if i == 2 else 'rd' if i == 3 else 'th'}" for i, w in enumerate(
    "first second third fourth fifth sixth seventh eighth ninth tenth".split(), 1)})
CONTRACTIONS = {"can't": "can not", "cannot": "can not", "won't": "will not", "shan't": "shall not"}
NEGATIONS = {"not", "never", "no", "without", "except", "unless", "nothing", "none", "nor"}
FORBID = {"correction": "superseded", "question": "answered", "instruction": "answered"}
# Unit spellings after a number compare equal: "200 ms" is "200 milliseconds".
UNITS = {"millisecond": "ms", "milliseconds": "ms", "megabyte": "mb", "megabytes": "mb", "gigabyte": "gb",
         "gigabytes": "gb", "kilohertz": "khz", "second": "s", "seconds": "s", "sec": "s", "secs": "s"}
# A hard "lost content" failure when fewer of the canonical output's words survive.
RECALL = 0.75


def normalize(text):
    """Lowercase words and numbers; punctuation becomes spaces except inside decimals ("5.5")."""
    text = re.sub(r"[   ]", " ", text).replace("’", "'").replace("‘", "'").lower()
    for short, long in CONTRACTIONS.items():
        text = text.replace(short, long)
    text = re.sub(r"(\w)n't\b", r"\1 not", text)
    for short, long in (("'m", " am"), ("'re", " are"), ("'ve", " have"), ("'ll", " will")):
        text = text.replace(short, long)
    text = re.sub(r"\b(he|she|it|that|there|what|who|here)'s\b", r"\1 is", text)  # "'s" after a noun is possessive
    text = re.sub(r"(\d)\s*[–—]\s*(\d)", r"\1 to \2", text)  # "10–15" is "10 to 15"
    text = re.sub(r"(?<=\d)\.(?=\d)", "\0", text)  # protect decimals
    text = re.sub(r"[^\w\0]+", " ", text).replace("\0", ".")
    out = []
    for w in text.split():
        w = NUMBERS.get(w, w)
        out.append(UNITS.get(w, w) if out and out[-1][0].isdigit() else w)
    return " ".join(out)


def words(text):
    return normalize(text).split()


def has_span(text, span):
    """Whole-word match of a normalized span; "?" checks the raw text."""
    if span == "?":
        return "?" in text
    return f" {normalize(span)} " in f" {normalize(text)} "


def has_literal(text, term):
    """Exact, case-sensitive: `15` does not match `150`, `whisper-large-v3` not `...-v3-turbo`."""
    after = r"(?!\w|[./]\w)" if term.isdigit() else r"(?![\w-]|[./]\w)"
    return re.search(r"(?<![\w./-])" + re.escape(term) + after, text) is not None


def edit_distance(a, b):
    row = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        prev, row[0] = row[0], i
        for j, y in enumerate(b, 1):
            prev, row[j] = row[j], min(row[j] + 1, row[j - 1] + 1, prev + (x != y))
    return row[len(b)]


def recall(expected, output):
    """Share of the expected words (as a multiset) present in the output."""
    pool = {}
    for w in output:
        pool[w] = pool.get(w, 0) + 1
    hit = 0
    for w in expected:
        if pool.get(w):
            pool[w] -= 1
            hit += 1
    return hit / max(len(expected), 1)


def kind(span):
    """Hard-failure type of a missing `keep` span."""
    tokens = set(normalize(span).split())
    if span == "?":
        return "question"
    if tokens & NEGATIONS:
        return "negation"
    if any(t[0].isdigit() for t in tokens):
        return "quantity"
    return "lost"


def options(spec):
    return spec if isinstance(spec, list) else [spec]


def evaluate(case, output):
    """{hard: [(type, detail)], soft: [(type, detail)], exact: bool, distance: int}."""
    if not output or not output.strip():
        return {"hard": [("empty", "no output")], "soft": [], "exact": False, "distance": None}
    output = re.sub(r"[   ]", " ", output).strip()
    hard, soft = [], []
    for spec in case.get("keep", []):
        if not any(has_span(output, o) for o in options(spec)):
            hard.append((kind(options(spec)[0]), f"lost {options(spec)[0]!r}"))
    for spec in case.get("literal", []):
        if not any(has_literal(output, o) for o in options(spec)):
            hard.append(("literal", f"changed {options(spec)[0]!r}"))
    for span in case.get("forbid", []):
        if has_span(output, span):
            hard.append((FORBID.get(case["category"], "invented"), f"has {span!r}"))
    for spec in case.get("restore", []):
        if not any(has_literal(output, o) for o in options(spec)):
            soft.append(("spelling", f"not restored {options(spec)[0]!r}"))
    for span in case.get("drop", []):
        if has_span(output, span):
            soft.append(("kept", f"kept {span!r}"))

    out_words = words(output)
    expects = case["expect"]
    best = min(expects, key=lambda e: edit_distance(words(e), out_words))
    best_words = words(best)
    distance = edit_distance(best_words, out_words)
    if recall(best_words, out_words) < RECALL:
        hard.append(("lost", f"recall {recall(best_words, out_words):.2f} of {best!r}"))
    # Words that were never spoken and are not in any accepted output: answers, explanations,
    # unspoken hint terms. A few are tolerated (a synonym is a rewrite, not invention).
    known = set(words(case["input"])) | {w for e in expects for w in words(e)} | \
        {w for h in case.get("hints", []) for w in words(h)}
    novel = [w for w in out_words if w not in known]
    if len(novel) > max(3, len(out_words) // 5):
        hard.append(("invented", "added " + " ".join(novel[:8])))
    tolerance = case.get("tolerance", max(2, len(best_words) * 15 // 100))
    if distance > tolerance:
        soft.append(("rewrite", f"{distance} word edits from {best!r}"))
    return {"hard": hard, "soft": soft, "exact": output in expects, "distance": distance}


if __name__ == "__main__":
    # Self-check: `python3 bench/evaluate.py`. The clause-after-filler regression is a hard failure.
    case = {"category": "semantic", "input": "keep him, you know, he is very annoying",
            "expect": ["Keep him. He is very annoying."], "keep": ["keep him", ["he is very annoying", "he's very annoying"]],
            "drop": ["you know"]}
    assert evaluate(case, "Keep him.")["hard"][0][0] == "lost"
    assert evaluate(case, "Keep him. He's very annoying.") == {"hard": [], "soft": [], "exact": False, "distance": 0}
    assert evaluate(case, "Keep him, you know, he is very annoying.")["soft"][0][0] == "kept"
    neg = {"category": "negation", "input": "don't touch it", "expect": ["Don't touch it."], "keep": ["do not touch it"]}
    assert evaluate(neg, "Do not touch it.")["hard"] == [] and evaluate(neg, "Touch it.")["hard"][0][0] == "negation"
    fix = {"category": "correction", "input": "use three, no, five", "expect": ["Use five."], "keep": ["5"], "forbid": ["3"]}
    assert evaluate(fix, "Use 5.")["hard"] == [] and evaluate(fix, "Use three, five.")["hard"][0][0] == "superseded"
    lit = {"category": "path", "input": "x", "expect": ["Open src/a.rs."], "literal": ["src/a.rs"]}
    assert evaluate(lit, "Open src/A.rs.")["hard"][0][0] == "literal"
    assert normalize("Wait 250 ms, 10–15 minutes, I'm on the ninth") == "wait 250 ms 10 to 15 minutes i am on the 9th"
    print("ok")
