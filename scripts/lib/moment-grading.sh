#!/usr/bin/env bash
# The grading of a figure a user-facing page states in words, and the class a
# page declares for its numbers to resolve against. Sourced, never run, and
# shared by check-budget-drift.sh, which grades README.md and
# docs/performance.md with it, and check-budget-drift-sweep.sh, which asks it
# which figures that grading reaches.
#
# The sweep asks rather than deciding for itself because the two answers have
# to be one answer: the sweep took every figure equal to any recorded seat,
# and once a re-seat made three bare-Neovim readings equal a cell they entered
# its population as figures the check was expected to grade, which the check
# was right not to -- the reading beside view's own belongs to the engine and
# no cell records it. Asking it only which figures it resolves is the
# opposite error, and it is the one that costs the sweep its job: a
# population that is by construction what the checker grades can never find a
# recorded value the checker grades nothing for, which is the blind spot the
# sweep exists to report.
#
# So the grading answers for every figure, in three verdicts: it resolves the
# figure to a cell, it excludes the figure on a ground one of its own rules
# states, or it reaches neither and the figure equals a recorded seat, which
# is unaccounted for and is a finding.
#
# Sets MOMENT_CLASS_AWK and MOMENT_GRADE_AWK. The grading program takes the
# seat table as its first input and the page as its second, with -v page (the
# name to report), -v fallback (the class the page declares) and -v mode:
# `grade` prints one finding per figure whose page and seat disagree,
# `classify` prints
# `CLS<TAB>site<TAB>line<TAB>index<TAB>value<TAB>resolved:<cell>` --
# or `excluded:<ground>`, or `unaccounted` -- for every figure of the
# population, where the index counts the figure tokens of that line.

# The class a unit resolves against when it names none. The page declares it
# in its own words and this reads that declaration, because a default kept in
# a script instead would be a class the reader of the page is never told about.
#
# Sentence scope, over the page joined: the declaration is prose and wraps
# wherever the paragraph does, so a line-at-a-time read finds it on the line
# whose half of the sentence happens to carry the class name.
MOMENT_CLASS_AWK='
  { buf = buf $0 " " }
  END {
    n = split(buf, sentence, /\. /)
    for (i = 1; i <= n; i++) {
      if (sentence[i] !~ /default class/) { continue }
      m = split(sentence[i], part, "`")
      for (j = 2; j <= m; j += 2) {
        if (part[j] ~ /^[a-z0-9]+-[a-z0-9]+$/) { print part[j]; exit }
      }
    }
  }'

# What a figure is, whether a figure is a value this tree records at all, and
# the walk that prints one verdict per figure. Shared by the three programs
# below and, through them, by the sweep: the sweep tokenized a line for
# itself once, and a multiplier with its suffix glued on (`0.30x`) was a
# figure to the grading and not to the sweep, which numbered every figure
# after it on that line one place early and perturbed the wrong one.
GRADE_COMMON_AWK='
    # The markup a figure may be written inside, taken off the token the
    # split below hands over: `**1.58**`, `(0.30x)` and `[19.57,` are the
    # figure they wrap.
    function clean(t) {
      gsub(/[][`*~()>|]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    # A decimal, or a number carrying the suffix a multiplier or a percentage
    # is written with. An integer without one is not a figure: it is a count,
    # a year or a bar, and the digit edit the sweep makes has no last place
    # to land on.
    function figure(t) {
      return (t ~ /^-?[0-9]+\.[0-9]+[x%]?$/ || t ~ /^-?[0-9]+[x%]$/)
    }
    function bare(t) {
      sub(/[x%]$/, "", t)
      return t
    }
    # A unit written against the number it measures is two tokens. A rewrap
    # that closed one space left `1.58ms` outside the grading, outside the
    # classification and outside the sweep population at once, which is the
    # silence all three exist to refuse.
    function unglue(tok, out,   c, num) {
      out[1] = tok
      c = clean(tok)
      if (!match(c, /[0-9](ms|us|MB|GB|min|s)$/)) { return 1 }
      num = substr(c, 1, RSTART)
      if (!figure(num)) { return 1 }
      out[1] = num
      out[2] = substr(c, RSTART + 1)
      return 2
    }
    # The one split every reader of a line makes. Whitespace separates
    # tokens, a pipe is a token of its own so a table boundary and a
    # digit-adjacent `1.43|1.58` read the same way wherever they stand, and a
    # glued unit comes apart. `at` takes each token own offset in the line,
    # which is what lets the sweep rewrite the nth figure in place instead of
    # rebuilding the line around it -- rebuilt on single spaces, a tab became
    # one space and the walk that numbered the figures was a second walk
    # that could disagree with this one.
    function toks(line, w, at,   n, i, len, c, start, rawtok, np, part, k, off) {
      n = 0
      len = length(line)
      i = 1
      while (i <= len) {
        c = substr(line, i, 1)
        if (c ~ /[[:space:]]/) { i++; continue }
        if (c == "|") { n++; w[n] = "|"; at[n] = i; i++; continue }
        start = i
        while (i <= len) {
          c = substr(line, i, 1)
          if (c ~ /[[:space:]]/ || c == "|") { break }
          i++
        }
        rawtok = substr(line, start, i - start)
        np = unglue(rawtok, part)
        for (k = 1; k <= np; k++) {
          n++
          w[n] = part[k]
          off = index(rawtok, part[k])
          at[n] = (off > 0) ? start + off - 1 : start
        }
      }
      return n
    }
    # A percentage OF something is a share only where the something is a
    # population the sentence names. Read as a share wherever `of` followed
    # it, the test excluded the flood pace -- a recorded ratio a record run
    # moves -- and the gap between a reading and the bar beside it, on a
    # ground that described neither. `i` is the `of` own index.
    function share_object(t, i,   j, w) {
      if (clean(t[i]) != "of") { return 0 }
      for (j = i + 1; j <= i + 3; j++) {
        w = clean(t[j])
        if (w == "") { continue }
        if (w ~ /^(a|an|the|its|their|those|these|all|same)$/) { continue }
        return (w ~ /^(lines|samples|runs|draws|trials|keystrokes|rows|entries)$/)
      }
      return 0
    }
    function site() {
      return (page == "") ? file : page
    }
    # Any seat of any class, because a page quotes the class its own unit
    # names and the classification has to know a recorded value when it sees
    # one wherever it stands.
    function seat_any(num,   i, fmt) {
      fmt = "%." decimals(num) "f"
      for (i = 1; i <= nany; i++) {
        if (sprintf(fmt, anyval[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    function note(at, idx, verdict) {
      if (idx > 0) { verdict_of[at SUBSEP idx] = verdict }
    }
    # A bar is named on either side of the figure it bounds: a page writes
    # `a bar of 10%` as readily as `10% bar`, and a test reading only the
    # token after left the bar on both user pages carrying the ground of a
    # unit instead of its own. The look back stops at the end of a clause --
    # a sentence mark, a comma, a dash -- because a bar closing one clause
    # bounds nothing in the next, and reading past it excluded the ratio
    # standing two words after `the 16 ms bar;`.
    function bounded(t, i, lo,   j, w) {
      if (clean(t[i + 1]) ~ /^(bar|bars|budget|bound|frame)$/) { return 1 }
      for (j = i - 1; j >= i - 2 && j >= lo; j--) {
        w = t[j]
        if (w == "\001" || w == "--" || w ~ /[,;:]$/) { return 0 }
        if (clean(w) ~ /^(bar|bars|budget|bound|frame)$/) { return 1 }
      }
      return 0
    }
    # The one place a sentence end is marked. A sentence ends at `. ` and at
    # a period the line end follows: these pages wrap at 80 characters, so most
    # sentences end at a line end, and a boundary read inside a line only let
    # a bound, a cell id and a transport denial in one sentence reach the
    # figure in the next. A decimal keeps its period, since no space and no
    # line end follows it inside a number; neither user page nor the
    # benchmarking page writes an abbreviation (`e.g.`, `vs.`) or a
    # backticked period at a line end, so no other spelling reaches the mark.
    function mark_sentences(line) {
      gsub(/\. /, " \001 ", line)
      sub(/\.[[:space:]]*$/, " \001", line)
      return line
    }
    function unit_of(tok) {
      if (tok == "ms") { return "ms" }
      if (tok == "MB") { return "MB" }
      if (tok == "us" || tok == "\xc2\xb5s") { return "us" }
      return ""
    }
    # The words a page marks a figure as a difference with, which is what
    # tells a gap between two readings from a reading of its own, each with
    # the side view own reading falls on: `larger` where the word says view
    # holds the bigger of the two, `smaller` where it says view holds the
    # lesser, `either` where the word states a magnitude and no direction at
    # all. A recomputation reading the magnitude alone passed `1.5 ms ahead`
    # on a page where view stands behind -- the same figure with its claim
    # reversed. `over` and `under` state a direction in English and are not
    # on this list: both pages write them as prepositions beside a figure
    # (`53.9 ms under view`, `9.0 percent over the 1.0 bar`), where reading
    # them as a difference recomputes a reading against its own pair.
    function direction_of(t) {
      if (t ~ /^(more|behind|later|past|slower)$/) { return "larger" }
      if (t ~ /^(less|fewer|ahead|earlier|faster)$/) { return "smaller" }
      if (t == "further") { return "either" }
      return ""
    }
    function difference_word(t) {
      return (direction_of(t) != "")
    }
    # Which of the two readings a difference is taken from is the larger,
    # empty where they are level and no direction word can disagree.
    function larger_of(v, e) {
      if (v + 0 > e + 0) { return "larger" }
      if (v + 0 < e + 0) { return "smaller" }
      return ""
    }
    # The bound a clause names after the word marking a figure a difference:
    # `past the 16 ms frame` writes the unit between the number and the word
    # where `than the 1.25 bar` writes none. Scoped to what follows that
    # word, because a sentence names the frame it sits inside before stating
    # a gap that is not from it -- the staleness paragraph writes both.
    function bound_after(t, a, b,   j, num) {
      for (j = a; j <= b; j++) {
        if (t[j] == "\001") { return "" }
        num = clean(t[j])
        if (num !~ /^-?[0-9]+(\.[0-9]+)?$/) { continue }
        if (bounded(t, j, a)) { return num }
        if (unit_of(clean(t[j + 1])) != "" \
            && clean(t[j + 2]) ~ /^(bar|bars|budget|bound|frame)$/) { return num }
      }
      return ""
    }
    # A ground that belongs to a whole line -- a fenced block, a ledger array,
    # a paragraph naming no cell -- is recorded against the line, so no
    # figure of it needs an index of its own.
    function note_line(at, ground) {
      if (ground_of[at] == "") { ground_of[at] = ground }
    }
    function classify_report(   ln, i, m, w, at, tok, idx, v) {
      for (ln = 1; ln <= lastline; ln++) {
        if (!(ln in scoped)) { continue }
        m = toks(raw[ln], w, at)
        idx = 0
        for (i = 1; i <= m; i++) {
          tok = clean(w[i])
          if (!figure(tok)) { continue }
          idx++
          v = verdict_of[ln SUBSEP idx]
          # The ground of the line outranks a token exclusion and never a
          # resolution: a sample inside a fence states no moment, and
          # reporting it as a sentence that names none says the ground of
          # every other figure in the block rather than its own.
          if (ground_of[ln] != "" && v !~ /^resolved/) {
            v = "excluded:" ground_of[ln]
          }
          # The population is what the checker resolves and what a reader
          # could mistake for a reading: a figure equal to no seat of any
          # class states nothing a record run can leave standing, and a
          # ground for it is a line of audit nobody has to read.
          if (v !~ /^resolved/ && !seat_any(bare(tok))) { continue }
          if (v == "") { v = "unaccounted" }
          printf "CLS\t%s\t%d\t%d\t%s\t%s\n", site(), ln, idx, tok, v
        }
      }
    }
'

MOMENT_GRADE_AWK="$GRADE_COMMON_AWK"'
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      cellseen[f[3]] = 1
      anyval[++nany] = f[4]
      if (index(fixtures[f[1] SUBSEP f[3]], " " f[2] " ") == 0) {
        fixtures[f[1] SUBSEP f[3]] = fixtures[f[1] SUBSEP f[3]] " " f[2] " "
      }
      next
    }
    # A fenced block is a sample of a file, never a reading of a cell, and
    # the whole page is in scope otherwise.
    mode == "classify" {
      raw[FNR] = $0
      scoped[FNR] = 1
      lastline = FNR
      if ($0 ~ /^[[:space:]]*```/) { fenced = !fenced }
      else if (fenced) { note_line(FNR, "a fenced block") }
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    # The words these pages name a fixture in, which include the one
    # docs/benchmarking.md has no use for: a page written for a person says
    # your config where the maintainer page says login-shaped.
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|Plugin-free/) { named = named " minimal " }
      if (text ~ /15-plugin/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|your config/) { named = named " user " }
      return named
    }
    # The moment a sentence states and the cell that records it. These pages
    # may name no identifier -- the identifier rule refuses one here -- so
    # the words are the only anchor a figure has.
    function moment_of(text) {
      if (text ~ /predicted glyph|glyph it expects|character it expects/) {
        return "echo_speculated.speculated_paint_p99_ms"
      }
      if (text ~ /keypress to glyph|worst keystroke|keystroke in a thousand/) {
        return "echo.view_p99_ms"
      }
      if (text ~ /stale/) { return "scroll.staleness_p99_ms" }
      if (text ~ /cadence/) { return "flood.cadence_p99_ms" }
      if (text ~ /matching results/) { return "picker.match_paint_p99_ms" }
      if (text ~ /first page of results/) { return "picker.first_page_p99_ms" }
      if (text ~ /worst launch/) { return "startup.first_frame_cold_ms" }
      if (text ~ /own process holds/) { return "memory.pss_mb" }
      # The engine mark, quoted on both pages with the word the pages write
      # it in and on neither with an identifier. The metric is itself a
      # difference, so the sentence states it in difference words and the
      # pick below reads those words as the moment rather than as a gap
      # between two published readings.
      if (text ~ /started[^ ]* mark/) { return "startup.server_delta_ms" }
      return ""
    }
    # The ratio cell of a moment. The milliseconds and the gap a reader
    # thinks about in percent are two cells of one moment, and the unit the
    # figure carries picks between them: a percentage or a bare multiplier
    # the ratio, a millisecond the absolute. A moment whose pairing records
    # no ratio returns none here, so the percentage beside it is excluded
    # rather than resolved against a sibling cell.
    function ratio_of(text) {
      if (text ~ /predicted glyph|glyph it expects|character it expects/) {
        return "echo_speculated.speculated_ratio_p50"
      }
      if (text ~ /keypress to glyph|worst keystroke|keystroke in a thousand/) {
        return "echo.ratio_p50"
      }
      if (text ~ /stale/) { return "scroll.ratio_p50" }
      if (text ~ /cadence/) { return "flood.cadence_p99_ratio" }
      # The flood the screen drains, which the pages state in percent and
      # never in milliseconds: the pace is a ratio of what the two sides
      # drain in one window and no cell records either side alone.
      if (text ~ /drains the flood/) { return "flood.pace_ratio" }
      if (text ~ /worst launch/) { return "startup.first_frame_ratio_p99" }
      # The settled screen, which the pages state as a gap and never as an
      # absolute: no cell records the milliseconds either side of it, so
      # this moment has a ratio cell and no millisecond one.
      if (text ~ /screen ready|screen you can start working in/) {
        return "startup.settled_ratio_p50"
      }
      return ""
    }
    # The number a ratio-shaped figure states, and the empty string for
    # anything else: a percentage and a bare multiplier carry their suffix,
    # which is what tells them from the milliseconds beside them.
    function ratio_tok(t) {
      if (t !~ /^-?[0-9]+(\.[0-9]+)?[x%]$/) { return "" }
      sub(/[x%]$/, "", t)
      return t
    }
    function cell_unit(cell) {
      if (cell ~ /_ms$/) { return "ms" }
      if (cell ~ /_mb$/) { return "MB" }
      if (cell ~ /_us$/) { return "us" }
      return ""
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # A percentage states the same ratio as its distance from 1, which is
    # how these pages write a gap a reader thinks about in percent, and how
    # docs/benchmarking.md is already read.
    function seated_pct(num, vals,   n, v, i, fmt, off) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        off = (v[i] > 1 ? v[i] - 1 : 1 - v[i]) * 100
        if (sprintf(fmt, off) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # The grounds these rules state for every figure of a sentence except the
    # ones they resolve. They are the same tests the pick loop above makes, in
    # the same order, said out loud: a reader of the sweep report accepts an
    # exclusion or does not, and cannot do either with a figure dropped in
    # silence.
    function grounds(a, b, pick, cell, rpick, rcell,   i, num, nxt, tail, why) {
      for (i = a; i <= b; i++) {
        if (i == pick || i == rpick || ti[i] == 0) { continue }
        num = clean(tk[i])
        nxt = clean(tk[i + 1])
        tail = (unit_of(nxt) != "") ? clean(tk[i + 2]) : nxt
        if (ratio_tok(num) != "") {
          # A percentage states the gap itself, so the difference words that
          # mark a millisecond as neither side reading never reach it:
          # `11% behind` is the ratio and `1.5 ms behind` is a gap between
          # two published readings.
          if (bounded(tk, i, a)) {
            why = "a bound, not a reading"
          } else if (num ~ /%$/ && nxt == "of" && share_object(tk, i + 1)) {
            why = "a share of a population, not a ratio"
          } else if (cell == "" && rcell == "") {
            # A figure the vocabulary reaches no moment for is not a figure
            # a rule says is no reading. Reported as an exclusion, it read
            # the same as a stale reading whose sentence had been reworded,
            # which is the state the sweep exists to report: it goes to
            # `unaccounted` and the sweep names it.
            continue
          } else if (rcell == "") {
            why = "a unit no cell of that moment is recorded in"
          } else if (rpick > 0 && i > rpick) {
            why = "the bare-engine reading beside view own"
          } else { continue }
          note(tl[i], ti[i], "excluded:" why)
          continue
        }
        # A difference is recomputed by diffs() above, so it carries no
        # ground here: one it could not recompute is a figure the grading
        # missed, which is the sweep to report and not a rule to state.
        if (difference_word(tail) && cell !~ /_delta_ms$/) { continue }
        if (bounded(tk, i, a)) {
          why = "a bound, not a reading"
        } else if (cell == "" && rcell == "") {
          continue
        } else if (cell == "" || unit_of(nxt) != cell_unit(cell)) {
          why = "a unit no cell of that moment is recorded in"
        } else if (pick > 0 && i > pick) {
          why = "the bare-engine reading beside view own"
        } else { continue }
        note(tl[i], ti[i], "excluded:" why)
      }
    }
    # The two readings a page publishes for one moment, view own first. A
    # table row states them in two columns of one sentence and a paragraph
    # states them in one, and the sentence writing the gap down states
    # neither, so a full pair is held under the moment it belongs to.
    function pair_scan(a, b,   i, num, nxt, n) {
      n = 0
      pair1 = ""
      pair2 = ""
      for (i = a; i <= b; i++) {
        num = clean(tk[i])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        nxt = clean(tk[i + 1])
        if (unit_of(nxt) == "") { continue }
        if (bounded(tk, i, a)) { continue }
        if (difference_word(clean(tk[i + 2]))) { continue }
        n++
        if (n == 1) { pair1 = num; continue }
        pair2 = num
        return 2
      }
      return n
    }
    # A figure a sentence states as a difference is recomputed from the two
    # readings it is the difference of: view own and the engine paired with
    # it, or view own and the bound the clause names. Left as a ground
    # instead, it stood on three sentences with nothing to move it when a
    # record run moved what it was taken from, which is what the percentage
    # beside the bar was already recomputed against. A difference whose two
    # operands are not both on the page is graded by nothing and goes to the
    # sweep as unaccounted.
    function diffs(a, b, key, npair, cell,   i, num, nxt, u, at, v, e, want, fmt, word, side) {
      v = (npair == 2) ? pair1 : ((key in pairv) ? pairv[key] : "")
      for (i = a; i <= b; i++) {
        num = clean(tk[i])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        if (cell ~ /_delta_ms$/) { continue }
        nxt = clean(tk[i + 1])
        u = unit_of(nxt)
        at = (u != "") ? i + 2 : i + 1
        word = clean(tk[at])
        if (!difference_word(word)) { continue }
        e = bound_after(tk, at + 1, b)
        if (e == "") { e = (npair == 2) ? pair2 : ((key in paire) ? paire[key] : "") }
        if (v == "" || e == "") { continue }
        want = (v + 0 > e + 0) ? v - e : e - v
        fmt = "%." decimals(num) "f"
        if (mode == "classify") {
          note(tl[i], ti[i], "resolved:" ((key != "") ? key : "the pair its own sentence states"))
          continue
        }
        # The direction before the magnitude: a gap of the right size stated
        # the wrong way round is the claim reversed, and a reader takes the
        # word rather than the arithmetic.
        side = larger_of(v, e)
        if (side != "" && direction_of(word) != "either" \
            && direction_of(word) != side) {
          printf "BUDGET DRIFT FAIL: moment-direction %s:%d: %s is stated as %s, where %s is the %s of the two readings it is the difference of (%s and %s)\n",
            page, tl[i], num, word, v, side, v, e
          continue
        }
        if (sprintf(fmt, want) + 0 == num + 0) { continue }
        printf "BUDGET DRIFT FAIL: moment-difference %s:%d: %s is stated as the difference between %s and %s, and those two stand %s apart\n",
          page, tl[i], num, v, e, sprintf(fmt, want)
      }
    }
    # The bar a sentence names, as a ratio-shaped figure. The first one,
    # since a sentence states one bar over the reading it publishes.
    function bar_value(a, b,   i, num) {
      for (i = a; i <= b; i++) {
        num = ratio_tok(clean(tk[i]))
        if (num == "") { continue }
        if (bounded(tk, i, a)) { return num }
      }
      return ""
    }
    # A percentage left over in a sentence that resolves a ratio and names a
    # bar is the distance between the two, which is how both pages write a
    # bar view has missed: `11% behind against a bar of 10% -- a second bar
    # missed, by 1% of the round trip`. It is computed rather than taken on
    # trust, because a share ground described neither the figure nor what
    # would move it: the reading the sentence resolves minus the bar it
    # names, at the digits printed.
    function gaps(a, b, rpick,   i, num, barv, rv, want, fmt) {
      if (rpick == 0) { return }
      barv = bar_value(a, b)
      rv = ratio_tok(clean(tk[rpick]))
      if (barv == "" || rv == "") { return }
      want = (rv + 0 > barv + 0) ? rv - barv : barv - rv
      for (i = a; i <= b; i++) {
        if (i == rpick || ti[i] == 0) { continue }
        num = clean(tk[i])
        if (num !~ /%$/) { continue }
        if (bounded(tk, i, a)) { continue }
        if (clean(tk[i + 1]) == "of" && share_object(tk, i + 1)) { continue }
        num = ratio_tok(num)
        fmt = "%." decimals(num) "f"
        if (sprintf(fmt, want) + 0 == num + 0) {
          note(tl[i], ti[i], "excluded:a difference from the bar the sentence names")
          continue
        }
        if (mode != "classify") {
          printf "BUDGET DRIFT FAIL: moment-gap %s:%d: %s%% stands beside a reading and the %s%% bar the sentence names, and the distance between those two is %s%%\n",
            page, tl[i], num, barv, sprintf(fmt, want)
        }
      }
    }
    # One picked figure against the seat its moment holds. The ratio and the
    # milliseconds of one sentence are two readings of one moment and resolve
    # on the same class and the same fixture, so this says what a pick is
    # worth and the sentence says where it was measured.
    function resolve(at, cell, pct, klass_one, fixture,   num, held) {
      if (!((klass_one SUBSEP fixture SUBSEP cell) in seat)) { return }
      held = seat[klass_one SUBSEP fixture SUBSEP cell]
      if (mode == "classify") {
        note(tl[at], ti[at], "resolved:" cell)
        return
      }
      num = clean(tk[at])
      sub(/[x%]$/, "", num)
      if (pct ? seated_pct(num, held) : seated(num, held)) { return }
      printf "BUDGET DRIFT FAIL: moment-drift %s:%d: %s%s is quoted as the %s moment on %s %s, and the value recorded there does not round to it at the digits printed\n",
        page, tl[at], num, (pct ? "%" : ""), cell, klass_one, fixture
    }
    function sentence(a, b, ufx,   i, text, cell, rcell, cls, nc, klass,
                      klass_one, fx, nf, fixn, fixture, num, nxt, tail,
                      pick, rpick, rpct, at, want, key, npair) {
      if (b < a) { return }
      text = ""
      for (i = a; i <= b; i++) { text = text " " tk[i] }
      cell = moment_of(text)
      if (!(cell in cellseen)) { cell = "" }
      rcell = ratio_of(text)
      if (!(rcell in cellseen)) { rcell = "" }
      key = (cell != "") ? cell : rcell
      npair = pair_scan(a, b)
      if (npair == 2 && key != "") { pairv[key] = pair1; paire[key] = pair2 }
      diffs(a, b, key, npair, cell)
      if (cell == "" && rcell == "") {
        # The grounds in one place and in one order, so the most specific one
        # a figure meets is the one reported: `1.5 ms behind` is a difference
        # whether or not the sentence around it names a moment, and reporting
        # it as a sentence naming none said the ground of its neighbours.
        if (mode == "classify") { grounds(a, b, 0, "", 0, "") }
        return
      }
      # The first reading of the sentence is view own: these pages publish
      # paired numbers, and the bare-engine one beside it is an absolute
      # this tree records no cell for. A figure the sentence calls a
      # difference is neither side reading. The two picks are the two units
      # one moment is published in, and a sentence states either or both.
      pick = 0
      rpick = 0
      rpct = 0
      for (i = a; i <= b; i++) {
        num = clean(tk[i])
        nxt = clean(tk[i + 1])
        if (ratio_tok(num) != "") {
          if (rpick > 0 || rcell == "") { continue }
          if (bounded(tk, i, a)) { continue }
          if (num ~ /%$/ && nxt == "of" && share_object(tk, i + 1)) { continue }
          rpick = i
          rpct = (num ~ /%$/)
          continue
        }
        if (pick > 0 || cell == "") { continue }
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        tail = (unit_of(nxt) != "") ? clean(tk[i + 2]) : nxt
        if (difference_word(tail) && cell !~ /_delta_ms$/) { continue }
        if (bounded(tk, i, a)) { continue }
        if (unit_of(nxt) != cell_unit(cell)) { continue }
        pick = i
      }
      if (mode == "classify") { grounds(a, b, pick, cell, rpick, rcell) }
      # After the grounds, because the distance from the bar is the more
      # specific reason a percentage is left alone and the grounds above
      # would otherwise report it as the engine reading beside view own.
      gaps(a, b, rpick)
      if (pick == 0 && rpick == 0) { return }
      at = (pick > 0) ? pick : rpick
      want = (pick > 0) ? cell : rcell
      num = clean(tk[at])
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      if (nc == 1) { klass_one = klass[1] }
      else if (fallback != "") { klass_one = fallback }
      else {
        printf "BUDGET DRIFT FAIL: moment-default %s:%d: %s is quoted as the %s moment and the page declares no default class, so the host it was measured on is one no reader is told\n",
          page, tl[at], num, want
        return
      }
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf == 0) { nf = split(ufx, fixn, " ") }
      if (nf == 0) { nf = split(fixtures[klass_one SUBSEP want], fixn, " ") }
      if (nf != 1) {
        if (nf > 1) {
          printf "BUDGET DRIFT FAIL: moment-scope %s:%d: %s is quoted as the %s moment where the unit names %d fixtures, and a number resolves against one fixture or against none\n",
            page, tl[at], num, want, nf
        }
        return
      }
      fixture = fixn[1]
      # The sites and nothing else, so the sweep perturbs the readings this
      # grading picked: the engine reading beside them is a figure no rule
      # grades, and a population that took it by value alone reported the
      # check as blind to a number it was never resolving.
      if (pick > 0) { resolve(pick, cell, 0, klass_one, fixture) }
      if (rpick > 0) { resolve(rpick, rcell, rpct, klass_one, fixture) }
    }
    # A table row is one sentence spread over its columns here, not one per
    # column: the row states its moment in the label column and its reading
    # in the next, so splitting on the pipe would leave every figure in a
    # sentence naming no moment at all.
    function grade(   i, j, m, line, w, at, text, ufx, s0) {
      if (uc == 0) { return }
      text = ""
      for (i = 1; i <= uc; i++) { text = text " " ul[i] }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        line = mark_sentences(line)
        m = toks(line, w, at)
        for (j = 1; j <= m; j++) {
          # The column boundary is no word of the sentence here: a row states
          # its moment in the label column and its reading in the next, and
          # splitting on the pipe would leave every figure in a sentence
          # naming no moment at all.
          if (w[j] == "|") { continue }
          ntok++
          tk[ntok] = w[j]
          tl[ntok] = uno[i]
          ti[ntok] = figure(clean(w[j])) ? ++fcount[uno[i]] : 0
        }
      }
      ufx = fixtures_of(text)
      s0 = 1
      for (i = 1; i <= ntok + 1; i++) {
        if (i == ntok + 1 || tk[i] == "\001") {
          sentence(s0, i - 1, ufx)
          s0 = i + 1
        }
      }
      uc = 0
    }
    /^[[:space:]]*\|/ { grade(); ul[1] = $0; uno[1] = FNR; uc = 1; grade(); next }
    /^[[:space:]]*$/ { grade(); next }
    /^[[:space:]]*([-*+][[:space:]]|[0-9]+[.)][[:space:]])/ { grade() }
    { uc++; ul[uc] = $0; uno[uc] = FNR }
    END { grade(); if (mode == "classify") { classify_report() } }
'

# The grading of docs/benchmarking.md, where a number takes the nearest cell
# id before it in its own sentence. Same two inputs and same modes as the
# program above, with -v fallback the class that page declares.
RATIO_GRADE_AWK="$GRADE_COMMON_AWK"'
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      cellseen[f[3]] = 1
      anyval[++nany] = f[4]
      if (index(fixtures[f[1] SUBSEP f[3]], " " f[2] " ") == 0) {
        fixtures[f[1] SUBSEP f[3]] = fixtures[f[1] SUBSEP f[3]] " " f[2] " "
      }
      next
    }
    mode == "classify" {
      raw[FNR] = $0
      scoped[FNR] = 1
      lastline = FNR
      if ($0 ~ /^[[:space:]]*```/) { fenced = !fenced }
      else if (fenced) { note_line(FNR, "a fenced block") }
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    # The words the page writes a fixture in, beside the names the baselines
    # record it under. A page that says plugin-free means minimal, and the
    # rule has to read the words to grade the number standing next to them.
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|`minimal`|[.]minimal/) { named = named " minimal " }
      if (text ~ /15-plugin|`heavy`|[.]heavy/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|`user`|[.]user/) { named = named " user " }
      return named
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # A percentage states the same ratio as its distance from 1, which is how
    # the page writes a gap a reader thinks in percent about.
    function seated_pct(num, vals,   n, v, i, fmt, off) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        off = (v[i] > 1 ? v[i] - 1 : 1 - v[i]) * 100
        if (sprintf(fmt, off) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # The unit a cell is recorded in, read off the metric name rather than a
    # whitelist of the one unit the rule started with: a page quoting a
    # megabyte or a microsecond seat was left ungraded by a rule that only
    # knew about milliseconds, and writing the id beside it did not help.
    function unit_suffix(u) {
      if (u == "ms") { return "_ms$" }
      if (u == "MB") { return "_mb$" }
      return "_us$"
    }
    function scope(at, num, what) {
      printf "BUDGET DRIFT FAIL: ratio-scope %s:%d: %s is quoted where %s, and a number resolves against one class and one fixture or against neither\n",
        page, at, num, what
    }
    # One number resolves to one cell, never to the union of every cell the
    # unit names: a union passed a sibling metric and a sibling scenario as
    # the number the id beside it stands for, which is the same disagreement
    # between the identifier and the words the class and fixture rules were
    # minted for. So a number takes the nearest cell id before it in its own
    # sentence -- a table cell is a sentence, since a row states one column
    # at a time -- and the unit first id where its sentence names none.
    function grade(   i, j, k, m, n, text, part, w, at, num, nxt, pct, ntok,
                     ngraded, ncell, cell, namedcells, cls, nc, klass, fx, nf,
                     fixn, klass_one, fixture, seen, ok, after, ofat, line,
                     tok, cur, held, dbound, dword, want, fmt, side) {
      if (uc == 0) { return }
      text = ""
      for (i = 1; i <= uc; i++) { text = text " " ul[i] }
      ncell = 0
      namedcells = ""
      n = split(text, part, "`")
      for (i = 2; i <= n; i += 2) {
        if (part[i] !~ /^[a-z_]+\.[a-z_0-9]+$/) { continue }
        if (!(part[i] in cellseen)) { continue }
        if (index(namedcells, " " part[i] " ") > 0) { continue }
        ncell++
        cell[ncell] = part[i]
        namedcells = namedcells " " part[i] " "
      }
      # A unit naming no cell id states where the grading found no anchor,
      # which is the same state a reading whose sentence was reworded is in.
      # It is not a rule saying the figure is no reading, so nothing is
      # recorded here and a figure equal to a seat is left unaccounted for
      # the sweep to name.
      if (ncell == 0) {
        uc = 0
        return
      }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        line = mark_sentences(line)
        m = toks(line, w, at)
        for (j = 1; j <= m; j++) {
          ntok++
          # A row states one column at a time, so the boundary between two
          # of them ends the sentence a number resolves inside.
          tk[ntok] = (w[j] == "|") ? "\001" : w[j]
          tl[ntok] = uno[i]
          ti[ntok] = figure(clean(w[j])) ? ++fcount[uno[i]] : 0
        }
      }
      ngraded = 0
      cur = ""
      for (i = 1; i <= ntok; i++) {
        if (tk[i] == "\001") { cur = ""; continue }
        tok = clean(tk[i])
        if (tok ~ /^[a-z_]+\.[a-z_0-9]+$/ && (tok in cellseen)) { cur = tok; continue }
        num = tok
        nxt = clean(tk[i + 1])
        pct = 0
        dbound = ""
        dword = ""
        after = nxt
        ofat = i + 1
        # a leading minus is part of the number: a diagnostic records one, and
        # a regex without it left the page ungraded where the ledger was not
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && nxt == "%") { pct = 1; after = clean(tk[i + 2]); ofat = i + 2 }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        # an absolute resolves like a ratio where a cell of its own unit
        # stands beside it, and nowhere else: the column holding the paired
        # bare-engine reading names no cell of view own and states an
        # absolute this file records nothing for.
        else if (nxt ~ /^(ms|us|\xc2\xb5s|MB)$/) {
          if (cur == "" || cur !~ unit_suffix(nxt)) {
            # A row first column names the row subject, so a gap from a
            # bound named in another cell of that row is that subject own
            # distance from the bound. Read as an absolute naming no cell,
            # the gap the flood row states stood with nothing to move it.
            if (difference_word(clean(tk[i + 2])) && cell[1] ~ unit_suffix(nxt)) {
              dbound = bound_after(tk, i + 3, ntok)
              dword = clean(tk[i + 2])
            }
            if (dbound == "") {
              note(tl[i], ti[i],
                "excluded:an absolute in a sentence naming no cell of its unit")
              continue
            }
          }
        }
        else if (bounded(tk, i, 1)) {
          note(tl[i], ti[i], "excluded:a bound, not a reading")
          continue
        }
        else if (nxt ~ /^(s|min|GB)$/) {
          note(tl[i], ti[i], "excluded:a unit no cell is recorded in")
          continue
        }
        # a bar stated in percent, which the chain above reaches only for a
        # figure carrying no percent sign
        if (pct && bounded(tk, i, 1)) {
          note(tl[i], ti[i], "excluded:a bound, not a reading")
          continue
        }
        # a percentage OF something is a share of a population, not a ratio
        # stated as its distance from 1 -- and only where the something is a
        # population the sentence names
        if (pct && after == "of" && share_object(tk, ofat)) {
          note(tl[i], ti[i], "excluded:a share of a population, not a ratio")
          continue
        }
        sub(/x$/, "", num)
        ngraded++
        gnum[ngraded] = num
        gpct[ngraded] = pct
        gat[ngraded] = tl[i]
        gidx[ngraded] = ti[i]
        gbound[ngraded] = dbound
        gword[ngraded] = dword
        gcell[ngraded] = (cur != "") ? cur : cell[1]
      }
      if (ngraded == 0) { uc = 0; return }
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) {
        scope(gat[1], gnum[1], "the unit names " nc " classes (" cls ")")
        uc = 0
        return
      }
      if (nc == 1) { klass_one = klass[1] }
      else if (fallback != "") { klass_one = fallback }
      else {
        scope(gat[1], gnum[1], "the unit names no class and the page declares no default class")
        uc = 0
        return
      }
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf > 1) {
        scope(gat[1], gnum[1], "the unit names " nf " fixtures (" fx ")")
        uc = 0
        return
      }
      if (nf == 1) { fixture = fixn[1] }
      else {
        seen = ""
        for (k = 1; k <= ncell; k++) {
          n = split(fixtures[klass_one SUBSEP cell[k]], fixn, " ")
          for (i = 1; i <= n; i++) {
            if (index(seen, " " fixn[i] " ") == 0) { seen = seen " " fixn[i] " " }
          }
        }
        nf = split(seen, fixn, " ")
        if (nf != 1) {
          if (nf == 0) { uc = 0; return }
          scope(gat[1], gnum[1], klass_one " records" namedcells "on " nf " fixtures (" seen ") and the unit names none")
          uc = 0
          return
        }
        fixture = fixn[1]
      }
      for (i = 1; i <= ngraded; i++) {
        if (!((klass_one SUBSEP fixture SUBSEP gcell[i]) in seat)) {
          note(gat[i], gidx[i],
            "excluded:" klass_one " records no seat for " gcell[i] " on " fixture)
          continue
        }
        held = seat[klass_one SUBSEP fixture SUBSEP gcell[i]]
        if (gbound[i] != "") {
          want = (held + 0 > gbound[i] + 0) ? held - gbound[i] : gbound[i] - held
          fmt = "%." decimals(gnum[i]) "f"
          if (mode == "classify") {
            note(gat[i], gidx[i], "resolved:" gcell[i])
            continue
          }
          side = larger_of(held, gbound[i])
          if (side != "" && direction_of(gword[i]) != "either" \
              && direction_of(gword[i]) != side) {
            printf "BUDGET DRIFT FAIL: ratio-direction %s:%d: %s is stated as %s the %s bound this row names, where %s stands on the %s side of it on %s %s\n",
              page, gat[i], gnum[i], gword[i], gbound[i], gcell[i], side, klass_one, fixture
            continue
          }
          if (sprintf(fmt, want) + 0 == gnum[i] + 0) { continue }
          printf "BUDGET DRIFT FAIL: ratio-difference %s:%d: %s is stated as the distance from the %s bound this row names, and %s stands %s from it on %s %s\n",
            page, gat[i], gnum[i], gbound[i], gcell[i], sprintf(fmt, want), klass_one, fixture
          continue
        }
        ok = gpct[i] ? seated_pct(gnum[i], held) : seated(gnum[i], held)
        # The cell this number resolves to and not the cells its value
        # happens to equal: a figure standing beside one id while equalling
        # another id seat is a reading of the first, and a population that
        # named the second perturbed it as a value nothing here grades.
        if (mode == "classify") {
          note(gat[i], gidx[i], "resolved:" gcell[i])
          continue
        }
        if (!ok) {
          printf "BUDGET DRIFT FAIL: ratio-drift %s:%d: %s%s is quoted beside %s on %s %s, and the value recorded there does not round to it at the digits printed\n",
            page, gat[i], gnum[i], (gpct[i] ? "%" : ""), gcell[i], klass_one, fixture
        }
      }
      uc = 0
    }
    /^[[:space:]]*\|/ {
      grade()
      ul[1] = $0; uno[1] = FNR; uc = 1; grade()
      next
    }
    /^[[:space:]]*$/ { grade(); next }
    { uc++; ul[uc] = $0; uno[uc] = FNR }
    END { grade(); if (mode == "classify") { classify_report() } }
'

# The grading of the figures a [[shortfall]] states in its why, which resolve
# against the entry own class, scenario, fixture and metric. Takes the seat
# table and budgets.toml, with -v file the name to report and -v TRIALS_BAND
# the fraction a draw may sit from its seat. In population mode it reports
# the why figures alone.
WHY_GRADE_AWK="$GRADE_COMMON_AWK"'
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      anyval[++nany] = f[4]
      next
    }
    mode == "classify" { raw[FNR] = $0; lastline = FNR }
    function value(line,   v) {
      v = line
      sub(/^[a-z_]+ = /, "", v)
      sub(/^"/, "", v)
      sub(/"$/, "", v)
      return v
    }
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|[.]minimal/) { named = named " minimal " }
      if (text ~ /15-plugin|[.]heavy/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|[.]user/) { named = named " user " }
      return named
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # A percentage in a why states the distance from the bar the sentence
    # names, which is how a ledger entry writes a miss; the pages write the
    # distance from 1 instead, so both spellings are read and the sentence
    # decides which by naming a bar or not.
    function bar_of(w, m,   j, t, nx) {
      for (j = 1; j <= m; j++) {
        t = clean(w[j])
        if (t !~ /^[0-9]+(\.[0-9]+)?$/) { continue }
        nx = clean(w[j + 1])
        if (bounded(w, j, 1)) { return t }
        if (nx == "ms" && clean(w[j + 2]) ~ /^(bar|bars|budget|bound|frame)$/) { return t }
      }
      return ""
    }
    function off_pct(held, bar) {
      if (bar != "" && bar + 0 != 0) { held = held / bar }
      return (held > 1 ? held - 1 : 1 - held) * 100
    }
    function seated_pct(num, held, bar,   fmt) {
      fmt = "%." decimals(num) "f"
      return sprintf(fmt, off_pct(held, bar)) + 0 == num + 0
    }
    # Every figure a why states is the value of a cell that why names: an
    # entry names its own class, scenario, fixture and metric, so a number
    # written beside an identifier resolves exactly. A figure with no
    # identifier in its sentence is attributed to nothing and goes stale in
    # silence at the next record run, which is what the stale ratio did in
    # both places it stood.
    function sentence_verdict(text, at,   j, k, m, w, tok, cls, nc, klass,
                              fx, nf, fixn, klass_one, fixture, cellid, nids,
                              idname, idat, isid, num, nxt, pct, after, ofat,
                              held, ok, bar, want, idx, tat) {
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      klass_one = (nc == 1) ? klass[1] : class
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf > 1) { return }
      fixture = (nf == 1) ? fixn[1] : fixt
      m = toks(text, w, tat)
      bar = bar_of(w, m)
      nids = 0
      for (j = 1; j <= m; j++) {
        tok = clean(w[j])
        cellid = ""
        # a bare metric name belongs to the entry own scenario; a written
        # out scenario.metric names its own, since a why may settle a
        # question with a cell from a scenario the entry does not measure
        if (tok ~ /^[a-z_0-9]+$/ && ((klass_one SUBSEP fixture SUBSEP scen "." tok) in seat)) {
          cellid = scen "." tok
        } else if (tok ~ /^[a-z_]+\.[a-z_0-9]+$/ && ((klass_one SUBSEP fixture SUBSEP tok) in seat)) {
          cellid = tok
        }
        isid[j] = (cellid != "")
        if (cellid != "") { nids++; idname[nids] = cellid; idat[nids] = j }
      }
      for (j = 1; j <= m; j++) {
        if (isid[j]) { continue }
        num = clean(w[j])
        idx = figure(num) ? ++fidx : 0
        nxt = clean(w[j + 1])
        pct = 0
        after = nxt
        ofat = j + 1
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && (nxt == "%" || nxt == "percent")) {
          pct = 1; after = clean(w[j + 2]); ofat = j + 2
        }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        else if (bounded(w, j, 1)) {
          note(at, idx, "excluded:a bound, not a reading")
          continue
        }
        else if (nxt ~ /^(s|min|GB)$/) {
          note(at, idx, "excluded:a unit no cell is recorded in")
          continue
        }
        # a bar stated in percent, which the chain above reaches only for a
        # figure carrying no percent sign
        if (pct && bounded(w, j, 1)) {
          note(at, idx, "excluded:a bound, not a reading")
          continue
        }
        # a percentage OF something is a share of a population, and only
        # where the something is a population the sentence names
        if (pct && after == "of" && share_object(w, ofat)) {
          note(at, idx, "excluded:a share of a population, not a ratio")
          continue
        }
        sub(/x$/, "", num)
        if (nids == 0) {
          printf "BUDGET DRIFT FAIL: why-figure %s/%s.%s: %s:%d states %s%s in a sentence that names no cell, so the figure is attributed to nothing and a record run leaves it standing\n",
            klass_one, scen, fixture, file, at, num, (pct ? "%" : "")
          continue
        }
        want = idname[1]
        for (k = 1; k <= nids; k++) { if (idat[k] < j) { want = idname[k] } }
        held = seat[klass_one SUBSEP fixture SUBSEP want]
        ok = pct ? seated_pct(num, held, bar) : seated(num, held)
        if (mode == "classify") {
          note(at, idx, "resolved:" want)
          continue
        }
        if (!ok) {
          printf "BUDGET DRIFT FAIL: why-drift %s/%s.%s: %s:%d quotes %s%s beside %s, and the value recorded there does not round to it at the digits printed\n",
            klass_one, scen, fixture, file, at, num, (pct ? "%" : ""), want
        }
      }
    }
    # The figures of a why are numbered over the whole line, and the count is
    # taken beside the grading rather than inside it: a sentence the grading
    # returns from early -- two classes named, two fixtures -- graded none of
    # its figures and numbered none either, which moved every figure after it
    # on the line and hung a bound ground on a multiplier.
    function count_figures(text,   j, m, w, at, c) {
      m = toks(text, w, at)
      c = 0
      for (j = 1; j <= m; j++) { if (figure(clean(w[j]))) { c++ } }
      return c
    }
    function flush(   n, i, sent, base) {
      if (why == "" || scen == "" || metr == "" || class == "") { why = ""; return }
      n = split(why, sent, /\. /)
      base = 0
      for (i = 1; i <= n; i++) {
        fidx = base
        sentence_verdict(sent[i], whyat)
        base = base + count_figures(sent[i])
      }
      why = ""
    }
    # The draws a record run took are a field rather than a sentence, and
    # every member is a draw of the metric the entry seats. One further out
    # than the band is a figure of another quantity -- the paired arm own
    # milliseconds is the shape the ledger shipped -- parked where no cell
    # can grade it. The band is the loader own.
    function check_trials(line, at,   body, n, t, i, num, off, span) {
      # A draw is graded against a band around its entry seat rather than
      # against a site, so the classification states the ground and the sweep
      # moves it out of that band in a leg of its own.
      if (mode == "classify") { return }
      body = line
      sub(/^trials = /, "", body)
      gsub(/[][]/, "", body)
      if (acc == "") {
        printf "BUDGET DRIFT FAIL: trials-band %s/%s.%s: %s:%d lists the draws of an entry that states no accepted value, so the array is anchored to nothing\n",
          class, scen, fixt, file, at
        return
      }
      span = (acc < 0 ? -acc : acc) * TRIALS_BAND
      n = split(body, t, /,/)
      for (i = 1; i <= n; i++) {
        num = t[i]
        gsub(/[[:space:]]/, "", num)
        if (num !~ /^-?[0-9]+(\.[0-9]+)?$/) { continue }
        off = num - acc
        if (off < 0) { off = -off }
        if (off > span) {
          printf "BUDGET DRIFT FAIL: trials-band %s/%s.%s: %s:%d lists the trial %s, further from the accepted %s than a draw of this metric goes, so the array states a quantity this cell does not draw\n",
            class, scen, fixt, file, at, num, acc
        }
      }
    }
    /^\[\[/ {
      flush()
      block = ($0 ~ /shortfall/)
      scen = ""; fixt = ""; metr = ""; class = ""; why = ""; acc = ""
      next
    }
    !block { next }
    /^accepted = / { acc = value($0); next }
    /^trials = / {
      if (mode == "classify") {
        scoped[FNR] = 1
        note_line(FNR, "a trials draw, graded against the band of its entry")
      }
      check_trials($0, FNR)
      next
    }
    /^scenario = / { scen = value($0); next }
    /^fixture = / { fixt = value($0); next }
    /^metric = / { metr = value($0); next }
    /^class = / { class = value($0); next }
    /^why = / {
      why = value($0)
      whyat = FNR
      if (mode == "classify") { scoped[FNR] = 1 }
      flush()
      next
    }
    END { flush(); if (mode == "classify") { classify_report() } }
'
