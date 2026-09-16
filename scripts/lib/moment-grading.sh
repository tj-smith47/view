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
# no cell records it.
#
# Sets MOMENT_CLASS_AWK and MOMENT_GRADE_AWK. The grading program takes the
# seat table as its first input and the page as its second, with -v page (the
# name to report), -v fallback (the class the page declares) and -v mode:
# `grade` prints one finding per figure whose page and seat disagree,
# `population` prints `POP<TAB>line<TAB>value<TAB>cell` for every figure the
# grading resolves to a seat that agrees.

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

MOMENT_GRADE_AWK='
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      cellseen[f[3]] = 1
      if (index(fixtures[f[1] SUBSEP f[3]], " " f[2] " ") == 0) {
        fixtures[f[1] SUBSEP f[3]] = fixtures[f[1] SUBSEP f[3]] " " f[2] " "
      }
      next
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
      return ""
    }
    function cell_unit(cell) {
      if (cell ~ /_ms$/) { return "ms" }
      if (cell ~ /_mb$/) { return "MB" }
      if (cell ~ /_us$/) { return "us" }
      return ""
    }
    function unit_of(tok) {
      if (tok == "ms") { return "ms" }
      if (tok == "MB") { return "MB" }
      if (tok == "us" || tok == "\xc2\xb5s") { return "us" }
      return ""
    }
    function clean(t) {
      gsub(/[`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    function sentence(a, b, ufx,   i, text, cell, cls, nc, klass, klass_one,
                      fx, nf, fixn, fixture, num, nxt, tail, held, pick) {
      if (b < a) { return }
      text = ""
      for (i = a; i <= b; i++) { text = text " " tk[i] }
      cell = moment_of(text)
      if (cell == "" || !(cell in cellseen)) { return }
      # The first reading of the sentence is view own: these pages publish
      # paired numbers, and the bare-engine one beside it is an absolute
      # this tree records no cell for. A figure the sentence calls a
      # difference is neither side reading.
      pick = 0
      for (i = a; i <= b; i++) {
        num = clean(tk[i])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        nxt = clean(tk[i + 1])
        tail = (unit_of(nxt) != "") ? clean(tk[i + 2]) : nxt
        if (tail ~ /^(more|less|fewer|behind|ahead|earlier|later|further)$/) { continue }
        if (unit_of(nxt) != cell_unit(cell)) { continue }
        pick = i
        break
      }
      if (pick == 0) { return }
      num = clean(tk[pick])
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      if (nc == 1) { klass_one = klass[1] }
      else if (fallback != "") { klass_one = fallback }
      else {
        printf "BUDGET DRIFT FAIL: moment-default %s:%d: %s is quoted as the %s moment and the page declares no default class, so the host it was measured on is one no reader is told\n",
          page, tl[pick], num, cell
        return
      }
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf == 0) { nf = split(ufx, fixn, " ") }
      if (nf == 0) { nf = split(fixtures[klass_one SUBSEP cell], fixn, " ") }
      if (nf != 1) {
        if (nf > 1) {
          printf "BUDGET DRIFT FAIL: moment-scope %s:%d: %s is quoted as the %s moment where the unit names %d fixtures, and a number resolves against one fixture or against none\n",
            page, tl[pick], num, cell, nf
        }
        return
      }
      fixture = fixn[1]
      if (!((klass_one SUBSEP fixture SUBSEP cell) in seat)) { return }
      held = seat[klass_one SUBSEP fixture SUBSEP cell]
      # The site and nothing else, so the sweep perturbs the reading this
      # grading picked: the engine reading beside it is a figure no rule
      # grades, and a population that took it by value alone reported the
      # check as blind to a number it was never resolving.
      if (mode == "population") {
        if (seated(num, held)) { printf "POP\t%d\t%s\t%s\n", tl[pick], num, cell }
        return
      }
      if (!seated(num, held)) {
        printf "BUDGET DRIFT FAIL: moment-drift %s:%d: %s is quoted as the %s moment on %s %s, and the value recorded there does not round to it at the digits printed\n",
          page, tl[pick], num, cell, klass_one, fixture
      }
    }
    # A table row is one sentence spread over its columns here, not one per
    # column: the row states its moment in the label column and its reading
    # in the next, so splitting on the pipe would leave every figure in a
    # sentence naming no moment at all.
    function grade(   i, j, m, line, w, text, ufx, s0) {
      if (uc == 0) { return }
      text = ""
      for (i = 1; i <= uc; i++) { text = text " " ul[i] }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        gsub(/\|/, " ", line)
        gsub(/\. /, " \001 ", line)
        m = split(line, w, /[[:space:]]+/)
        for (j = 1; j <= m; j++) { ntok++; tk[ntok] = w[j]; tl[ntok] = uno[i] }
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
    END { grade() }
'
