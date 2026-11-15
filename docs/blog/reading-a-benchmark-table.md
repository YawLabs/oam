# Reading a benchmark table without fooling yourself

This post contains no numbers you have not already seen. Every figure it
discusses is published elsewhere on this blog -- in
<a href="https://oamjs.org/blog/why-oam/">Why Oam.js</a>, in
<a href="https://oamjs.org/blog/mcp-servers-as-sidecars/">MCP servers as
sidecars</a>, in
<a href="https://oamjs.org/blog/no-linux-arm64-yet/">Why there is no
linux-arm64 build yet</a> -- and this post does not restate them. It is
about what those posts already do, stated as rules you can apply to a table
this project has not published yet, or to somebody else's.

*The posts these rules come from quote `BENCHMARKS.md` and the two
conformance scorecards. Both are generated artefacts with their own
staleness rules; the figures in the linked posts carry their commits.*

## Rule one: a row measures one thing, and you have to find out what

Every benchmark row answers a question, and the question is usually narrower
than the row's label. The clearest example on this blog is already written
up at length: a row labelled with a protocol name may be measuring where the
protocol stack is installed rather than how fast the runtime starts it.
<a href="https://oamjs.org/blog/mcp-servers-as-sidecars/">That post</a>
states its own confound in a boxed note and names the row from the same run
that does not carry it.

The transferable move is the one that post models: when a row's label and
its measurement are not obviously the same thing, the honest response is not
to drop the row but to publish the confound beside it and point at the row
that isolates the variable. A table that never names a confound is not
measuring more -- it is disclosing less.

## Rule two: some rows are soft, and which ones is a property of the harness, not the number

A row's stability depends on what it makes the machine do. On this blog the
spawn-bound rows are called out as the softest in the set, for reasons
specific to how the harness runs: it stages the binary under test once
before timing and lets its competitors pay a per-iteration cost that oam has
already paid. That is a property of the harness, not of the runtimes, and
<a href="https://github.com/YawLabs/oam/blob/main/docs/why-oam.md"><code>docs/why-oam.md</code></a>
says which rows it affects and why.

The general form: before you trust a gap between two columns, ask whether
the harness treats the two columns the same way at every step. A row can be
perfectly measured and still be measuring the harness's staging policy.

## Rule three: a percentage is two numbers, and publishing one of them is the oldest trick in the field

<a href="https://oamjs.org/blog/why-oam/">Why Oam.js</a> spends most of its
length on this, which is why this post does not repeat it: the same scorecard
yields two different percentages depending on what you put in the
denominator, and the post publishes both plus the named populations excluded
from each. The rule to carry to other people's tables is the one that post's
title states -- ask what the ratio is over, and if the answer is not
published, the percentage is not evidence.

The sharper sub-case, also already written up there: a scorecard that
publishes a mediocre-looking surface figure alongside the list of what is
missing is making a stronger claim than one that publishes only a high
figure, because the list is falsifiable and the bare percentage is not.

## Rule four: the loss rows are the credibility

A comparison that names where its subject loses is doing the thing that
makes the win rows worth reading. This blog does it in three separate posts,
each naming the competing runtime that wins and on which rows.

The failure mode to watch for elsewhere is not a table that shows losses --
it is a table whose losses are all in rows the author has quietly decided
are the unimportant ones. Ask why those rows are unimportant, and whether
the same reasoning would be accepted if the losses had landed in the rows
the post leads with.

## Rule five: a number without a run is a number without an expiry

Every figure on this blog is cited with the commit and profile it came
from, and one post tells the story of a table that was wrong twice -- once
from measuring a stale build, once from a parser bug that inverted a memory
figure. The second one is the instructive case: it understated the project's
own advantage, and it was published.

The rule is not "trust this project's numbers." It is that a figure you
cannot re-derive is a figure you cannot check, which is why the house rule
in <code>oamjs.org</code>'s own README is to cite the run rather than the
number, and to refresh provenance in the same edit as the figure -- a fresh
number beside a stale commit hash reads as verified when it is not.

**What would make this post obsolete:** nothing here is dated. It would be
obsolete if the blog stopped citing runs for its numbers, which is the
property the rules exist to protect.

oam is beta: breaking changes before 1.0 are still possible, and there is no
LTS, so this is not the runtime for a service you are on call for.