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

# The grading of docs/benchmarking.md, where a number takes the nearest cell
# id before it in its own sentence. Same two inputs and same modes as the
# program above, with -v fallback the class that page declares.
RATIO_GRADE_AWK='
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
    function grade(   i, j, m, n, text, part, w, num, nxt, pct, ntok, ngraded,
                     ncell, cell, namedcells, cls, nc, klass, fx, nf, fixn,
                     klass_one, fixture, seen, ok, after, line, tok, cur, held) {
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
      if (ncell == 0) { uc = 0; return }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        gsub(/\|/, " \001 ", line)
        gsub(/\. /, " \001 ", line)
        m = split(line, w, /[[:space:]]+/)
        for (j = 1; j <= m; j++) { ntok++; tk[ntok] = w[j]; tl[ntok] = uno[i] }
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
        after = nxt
        # a leading minus is part of the number: a diagnostic records one, and
        # a regex without it left the page ungraded where the ledger was not
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && nxt == "%") { pct = 1; after = clean(tk[i + 2]) }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        # an absolute resolves like a ratio where a cell of its own unit
        # stands beside it, and nowhere else: the column holding the paired
        # bare-engine reading names no cell of view own and states an
        # absolute this file records nothing for.
        else if (nxt ~ /^(ms|us|\xc2\xb5s|MB)$/) {
          if (cur == "" || cur !~ unit_suffix(nxt)) { continue }
        }
        else if (nxt ~ /^(s|min|GB|bar|bars|budget|bound|frame)$/) { continue }
        # a percentage OF something is a share of a population, not a ratio
        # stated as its distance from 1
        if (pct && after == "of") { continue }
        sub(/x$/, "", num)
        ngraded++
        gnum[ngraded] = num
        gpct[ngraded] = pct
        gat[ngraded] = tl[i]
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
        if (!((klass_one SUBSEP fixture SUBSEP gcell[i]) in seat)) { continue }
        held = seat[klass_one SUBSEP fixture SUBSEP gcell[i]]
        ok = gpct[i] ? seated_pct(gnum[i], held) : seated(gnum[i], held)
        # The cell this number resolves to and not the cells its value
        # happens to equal: a figure standing beside one id while equalling
        # another id seat is a reading of the first, and a population that
        # named the second perturbed it as a value nothing here grades.
        if (mode == "population") {
          if (ok) { printf "POP\t%d\t%s\t%s\n", gat[i], gnum[i], gcell[i] }
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
    END { grade() }
'

# The grading of the figures a [[shortfall]] states in its why, which resolve
# against the entry own class, scenario, fixture and metric. Takes the seat
# table and budgets.toml, with -v file the name to report and -v TRIALS_BAND
# the fraction a draw may sit from its seat. In population mode it reports
# the why figures alone.
WHY_GRADE_AWK='
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      next
    }
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
    # A percentage in a why states the distance from the bar the sentence
    # names, which is how a ledger entry writes a miss; the pages write the
    # distance from 1 instead, so both spellings are read and the sentence
    # decides which by naming a bar or not.
    function bar_of(w, m,   j, t, nx) {
      for (j = 1; j <= m; j++) {
        t = clean(w[j])
        if (t !~ /^[0-9]+(\.[0-9]+)?$/) { continue }
        nx = clean(w[j + 1])
        if (nx ~ /^(bar|bars|budget|bound|frame)$/) { return t }
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
                              idname, idat, isid, num, nxt, pct, after, held,
                              ok, bar, want) {
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      klass_one = (nc == 1) ? klass[1] : class
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf > 1) { return }
      fixture = (nf == 1) ? fixn[1] : fixt
      m = split(text, w, /[[:space:]]+/)
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
        nxt = clean(w[j + 1])
        pct = 0
        after = nxt
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && (nxt == "%" || nxt == "percent")) {
          pct = 1; after = clean(w[j + 2])
        }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        else if (nxt ~ /^(s|min|GB|bar|bars|budget|bound|frame)$/) { continue }
        # a percentage OF something is a share of a population
        if (pct && after == "of") { continue }
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
        if (mode == "population") {
          if (ok) { printf "POP\t%d\t%s\t%s\n", at, num, want }
          continue
        }
        if (!ok) {
          printf "BUDGET DRIFT FAIL: why-drift %s/%s.%s: %s:%d quotes %s%s beside %s, and the value recorded there does not round to it at the digits printed\n",
            klass_one, scen, fixture, file, at, num, (pct ? "%" : ""), want
        }
      }
    }
    function flush(   n, i, sent) {
      if (why == "" || scen == "" || metr == "" || class == "") { why = ""; return }
      n = split(why, sent, /\. /)
      for (i = 1; i <= n; i++) { sentence_verdict(sent[i], whyat) }
      why = ""
    }
    # The draws a record run took are a field rather than a sentence, and
    # every member is a draw of the metric the entry seats. One further out
    # than the band is a figure of another quantity -- the paired arm own
    # milliseconds is the shape the ledger shipped -- parked where no cell
    # can grade it. The band is the loader own.
    function check_trials(line, at,   body, n, t, i, num, off, span) {
      # A draw is graded against a band around its entry seat rather than
      # against a site, so the sweep moves it out of that band itself and
      # asks this program for nothing.
      if (mode == "population") { return }
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
    /^trials = / { check_trials($0, FNR); next }
    /^scenario = / { scen = value($0); next }
    /^fixture = / { fixt = value($0); next }
    /^metric = / { metr = value($0); next }
    /^class = / { class = value($0); next }
    /^why = / { why = value($0); whyat = FNR; flush(); next }
    END { flush() }
'
