# Doc 33 — Why TARS Does Not Support MCP

Status: **Decision — settled 2026-07.** TARS ships **no MCP**, client or server, and
this is a choice we are proud of, not a TODO we are ashamed of. This is the ADR that
explains why — so that no future contributor, in a moment of fashion-induced weakness,
bolts it into the core "to look complete."

> The short version: MCP is the fourth reincarnation of an idea that has failed three
> times already. It doesn't remove complexity, it relocates it. As a *type* it is a
> flat bag of `Json → Text` with no way to compose two things into a third. As a piece
> of *infrastructure* it is HTTP, REST and RPC reinvented three decades late and worse —
> not even a competent microservice: not secure, not scalable, not battle-tested. Its only genuine virtue is
> that a handful of closed apps already know how to dial it. That is a distribution
> fact, not an engineering one. TARS keeps its own typed, composable spine and refuses
> to let this thing near it.

---

## 1. A short history of the universal plug, and the graveyard behind it

Every decade or so, software's finest minds gather to solve the same problem — *how do
I get my system to talk to your system?* — and every decade they reach for the same
answer: a universal middleware that will wrap the whole world in one protocol, after
which everything will simply, magically, talk.

In the 1990s it was **CORBA**. An interface definition language, object request
brokers, a wire protocol called IIOP, and the glorious promise that any object could
call any other object, in any language, on any machine, as if they were sitting in the
same room. It was going to change everything.

Then came **SOAP** and **WSDL** — an envelope so heavy it needed its own postman, and
a schema language so verbose that reading one out loud was considered a form of
punishment.

Then the **Enterprise Service Bus** and the whole **SOA** gold rush: a decade and an
ocean of consultancy money spent wrapping every creaking corporate system in a
"service" so that the Bus, sitting in the middle like a maharaja, could route messages
between them.

Where are they now? CORBA is a ghost story told to frighten graduates. SOAP is a
punchline. The ESB is a line item in a 2009 migration budget nobody wants to discuss.
All three died of exactly the same disease, and it is worth naming the disease now,
because we are about to watch it kill a fourth patient:

1. **The universal wrapper never removed the complexity — it just moved it.** From the
   caller, who now had it easy, onto some exhausted soul whose entire job was
   maintaining the wrapper.
2. **A simpler substrate turned up and ate them.** Plain HTTP, JSON, a text editor and
   a `curl` command did ninety per cent of the job with none of the ceremony, and the
   maharaja was quietly led out the back.

Hold that thought. It is, more or less, the whole review.

---

## 2. Enter MCP — and a look at how agents actually work now

The Model Context Protocol is 2024's reincarnation of the universal plug. On paper it
is tidy: a server advertises a list of tools, a client calls one by name, JSON goes in,
"content" comes back. The ecosystem is genuinely large — the public directories list
tens of thousands of servers (a much smaller number once you dedupe), and SDK downloads
run to the tens of millions a month. It has, as its evangelists never tire of saying,
become "the USB-C port of AI."

So before we put it on the bench, let us watch what a *capable* agent — the thing we
are actually building for — does when you hand it a real task.

It asks for a shell. It types `gh pr view 412 --json files`. It writes
`SELECT id, total FROM orders WHERE status = 'stuck'`. It `curl`s the Stripe API with
the key it read from the process environment. It reads the OpenAPI spec, or the SDK's
docstrings, and it just… calls the thing.

What it does *not* do is ask you for a pre-digested catalogue of 400 tool descriptions
so it can guess which one to invoke. The frontier coding agents already route around
MCP entirely, because a shell and a good SDK is a stronger, faster, more honest
interface than a bag of JSON-RPC wrappers. Keep that picture in your head — the agent
happily typing `gh` — because everything that follows is just an unpacking of why that
picture wins.

---

## 3. So who is actually succeeding with it? (No clean success case)

If MCP were pulling its weight you would expect a flagship win — a deployment where the
*protocol itself* was the reason it worked. There isn't one. There are only two kinds
of "success," and neither is a win for MCP:

- **Block's Goose** wrapped internal systems — DataHub metadata, a service registry, an
  incident tool — as MCP servers and let engineers hit them all through one
  natural-language door. Lovely. But the win there is *"one door onto internal systems
  that never had a decent API,"* and an internal gRPC gateway delivers precisely that.
  The protocol is incidental.
- **Cloudflare's MCP Demo Day** paraded Asana, Atlassian, Linear, PayPal, Sentry,
  Stripe and Webflow shipping remote MCP servers. The win there is *"now reachable from
  inside Claude."* That is defensive distribution — getting your product in front of a
  captive audience — not return on investment.

Every "success" collapses to *unified entry* or *reachable from a closed host*. Neither
is a property of MCP the abstraction. That is the tell. Nobody, anywhere, adopted this
thing because the abstraction was good.

---

## 4. The thesis

**MCP is an anti-pattern.** Not "immature," not "early." Anti-pattern: a shape that
looks like a solution and is in fact the problem wearing a lanyard. The rest of this
document is the case for the prosecution, in three parts — what it is as a type, what
it is as infrastructure, and why TARS in particular is the last system on earth that
needs it.

---

## 5. The type-level indictment (why it's ugly on paper)

Take MCP seriously *as a type* and it deflates in your hands:

```
Server  ≈  { tools : List<(Name, JsonSchema_in, Description)> }
call    :  (Name, Json) -> Result<Content[], Error>
Content =  Text | Image | Audio | ResourceLink | EmbeddedResource
```

The effective type of every tool is **`Json → Text`**. The schema is runtime metadata,
not a static type. The return is dominated by `Text`. `structuredContent` is a
bolted-on afterthought you have to opt into. Three consequences:

**5.1 — It is inert.** MCP contributes *zero* intelligence. Its entire operational
semantics are "a model reads a sentence of English and *guesses* which name to call."
The abstraction has no behaviour of its own; it is defined only relative to an external,
stochastic interpreter. Unplug the LLM and nothing is left but a phone book.

**5.2 — It is stringly-typed.** One verb, `call`, over a flat namespace. No coproduct
(you can't type "A *or* B"), no product (you can't type "A's output *is* B's input").
Every domain type is erased into JSON and prose, so you never hold a value you can
trust — you hold a blob you *hope* parses. This is the exact inverse of TARS's own
`json_decode`, whose whole contract is *"hand it a `T`, get back a valid `T` or a typed
error."* MCP hands you a name and returns prose you must re-parse, forever.

**5.3 — "Extensible" means wider, not deeper.** MCP's celebrated extensibility is
"append another `Json → Text` entry to the list." There is no combinator — no
`compose : Tool a b → Tool b c → Tool a c`. Two MCP tools cannot be joined into a third.
Composition happens only inside the model's head, imperatively, re-derived every single
turn. It is not an algebra; it is a set of morphisms with the compose button removed.
The entire point of a good abstraction is that its pieces click together. MCP threw that
away and kept the packaging.

**In fairness (the steelman):** MCP is more than `tools/call` — there are `resources`,
`prompts`, and a whiff of bidirectionality in `sampling` and `elicitation`. But these
are barely implemented in the wild, and they widen the surface without adding a compose
law. A bigger bag is still a bag.

---

## 6. Why not even as a client? (The Stripe test)

"Fine," says the advocate, "don't build servers — just *consume* other people's."
No. If you can write code, a direct call beats an MCP call on every axis that matters:
typed, in-process, sandboxable, no extra hop, no daemon, no schema round-trip. The
"high-value" servers — GitHub, Playwright, a database — are wrappers around tools that
are already excellent directly: `gh`, the Playwright library, a driver.

Run the **Stripe test**. *"Why not just call Stripe's public REST API — can't the agent
do that?"* Of course it can. A capable agent reads the OpenAPI and calls REST directly.
A Stripe MCP server adds only two things: **curation** (trimming hundreds of endpoints
to a few "safe" ones with LLM-friendly blurbs — a guardrail, not a capability) and
**holding the key** (out of the model's context). Both of which you do *better* in your
own code. Stripe is in fact the *worst* case for MCP, precisely because its REST API is
world-class — there is nothing left for the wrapper to add.

Which gives us the governing law, and it is a cruel one:

> **The value of an MCP server is inversely proportional to the capability of the agent
> calling it.**

The smarter the agent, the less a pre-chewed catalogue is worth. MCP is scaffolding for
a *weak* agent that can't yet read a doc, open a shell, and call an API. As agents get
strong — and they are getting strong at a frightening rate — the scaffolding becomes
dead weight. It is, structurally, a bet against the very thing everyone is racing to
build.

---

## 7. The practical case — and an honest note on evidence

Everything above is the *architectural* case, and a purist could wave it away. So it is
worth asking how MCP behaves in the wild. But first, a deliberate omission, because this
ADR has to be something we can stand behind:

> **A note on evidence, because this ADR has to be one we can stand behind.** Where we
> cite, we cite what we verified against a *primary* source: the **OWASP MCP Top 10** (a
> real OWASP Foundation project) and specific **NVD** CVE entries. What we deliberately
> do *not* lean on are the loose percentages that fill 2026 blog posts ("X% of servers
> vulnerable to Y"): they trace only to vendor syntheses whose underlying survey we
> could not open, and at least one widely-echoed "statistic" turned out, on inspection,
> to be a model extrapolation that then bounced around the low-authority web until it
> looked like a fact. So we anchor on the named framework and the CVE, and we flag the
> percentages as vendor-reported rather than asserting them. The relief is that the
> argument barely needs numbers anyway — the failure modes are *structural*, and OWASP
> has already catalogued them.

**7.1 — The adoption is a hostage situation, not a victory.** MCP is genuinely
everywhere — enterprises stand up gateways to expose Snowflake, Salesforce, the internal
wiki. But *ask them why.* Not one did it because the abstraction is elegant. They did it
because their staff live inside **closed hosts** — Cursor, Claude Desktop, Microsoft
Copilot — and those hosts dial only this one plug. No MCP endpoint, no access to your
own data. That is not a protocol winning on merit; it is a toll booth. "USB-C for AI" is
a nicer way of saying *"the only socket the appliance accepts."*

**7.2 — The security failure mode is structural — and OWASP has already catalogued it.**
You don't have to take my word for the failure classes: OWASP now publishes a dedicated
**MCP Top 10** (MCP01–MCP10:2025, a real OWASP Foundation project). Read the list —
Token & Secret Exposure, Excessive Agent Permissions, Tool Poisoning, Supply-Chain /
Dependency Tampering, Command Injection, Prompt Injection, Insufficient Auth, Lack of
Audit, Shadow MCP Servers, Context Over-Sharing — and it reads like a summary of this
document. Every entry is the *predictable* consequence of the design: an untyped server
that takes a stochastic model's output as control input, shipped as thousands of
community packages installed by name. Path traversal and command injection are what you
get when "the model asked for a file / a command and the wrapper obliged";
supply-chain typosquatting is what you get from "install `some-mcp-server` by a name you
half-remember," now with pre-authorised corporate credentials attached; prompt injection
through tool *descriptions* is a front door the protocol opened on purpose. And it is not
hypothetical — **CVE-2025-6514** (NVD; CVSS **9.6**; found by JFrog) is documented
OS-command-injection RCE on the *client* machine simply from connecting to an untrusted
MCP server. Vendor write-ups (e.g. Cycode) attach alarming prevalence percentages to all
this; we flag those as vendor-reported rather than lean on them, because a named OWASP
Top 10 and a CVSS-9.6 client RCE already make the point without a disputed number. None
of it is bad luck; it is the shape. It is also the entire reason TARS write-jails every
delegate — the teams who instead trusted the bag of tools are the ones explaining an
incident to their board.

**7.3 — It isn't even a competent microservice — it's HTTP and RPC, reinvented badly.**
Strip the branding and MCP is JSON-RPC over HTTP (or stdio) with a `tools/list`
discovery call bolted on top. Which is to say: it is **reinventing HTTP, REST and RPC**,
three decades late, and it has a long way to go before it matches what those already do
in their sleep. And it reinvents them *badly*: the moment it reaches for *stateful*
sessions it becomes an incompetent copy of the thing it cloned. Any protocol that pins
per-session state in server memory fights a plain load balancer — horizontal scaling
needs sticky routing, and a dropped connection takes the session, and with it the
model's context, down with it. REST solved exactly this in the 1990s by being
*stateless*; MCP un-solved it. That is not an exotic edge case; it is the first wall
anyone hits running a stateful protocol like a real service — a timeless property, no
2026 citation required. A middleware that fights the load balancer is not middleware.
It's a demo that went viral.

**7.4 — It is complexity conservation, dressed as innovation.** This is the load-bearing
point. MCP deletes no complexity — it *shovels* it, off the agent's desk and onto
whoever now maintains `Stripe-MCP-Server`, `DB-MCP-Server`, `K8s-MCP-Server`, forever,
chasing every upstream API change. Which is to say: it is the **ESB and the SOA
gateway**, exhumed. There are tens of thousands of APIs in the world; the plan is to
wrap them all, by hand, and keep them current? Of course not — which is exactly why the
protocol's own vendor actively maintains only a handful of reference servers and the
rest of the "ecosystem" is a landfill of weekend toys. Nobody wants to be the soul
maintaining the wrapper. They never did. That's why CORBA is dead.

---

## 8. Why TARS, specifically, doesn't even need the plug

Now the part that closes the loop. MCP exists because Claude Desktop and ChatGPT are
**black boxes floating outside your walls** — they physically cannot touch your
database, so you are forced to stand up a server inside your network and cut them a hole
to reach in through.

**TARS is not outside the walls. TARS is embedded in your own service or binary.** And
the moment you say that out loud, MCP's entire reason for existing evaporates:

- **Credentials?** TARS runs *inside* your backend — on the box that already holds the
  IAM role, next to the secret already in the process environment. The model emits
  intent; TARS's Rust underneath performs the call. The credential never enters the LLM
  context, because it never needed to leave the process.
- **Connection pools?** TARS *is* part of your service. Your Redis pool, your Postgres
  pool — already there. The `Pipeline` reuses the host's pools directly. There is
  nothing for a separate MCP server to "manage."
- **Audit?** TARS carries its own MELT (metrics, events, logs, traces) and a typed error
  hierarchy, and because it executes **in-process** it can record the real call stack at
  near-zero cost — a thousand times clearer than tcpdumping JSON-RPC across a wire.

Wrapping TARS in MCP to reach the outside world safely is like pitching a plastic tent
inside a fortress. TARS *starts* behind the wall.

---

## 9. Verdict

- **As core architecture:** MCP is a leaky sieve with an ancestry of dead middleware and
  a rap sheet of live CVEs. Keeping it out of the core is not caution, it is hygiene.
  **Rejected.**
- **As an outer plug:** *if* the day comes that we genuinely want to serve users trapped
  inside Cursor or Claude Desktop, we may — grudgingly — bolt a throwaway MCP *server*
  adapter onto the outermost boundary: collapse a real `T` down to MCP's `Json → Text`,
  hand it over, and discard the collapse. It costs almost nothing (it's just JSON-RPC)
  and it earns nothing over plain gRPC except "the closed host already dials this
  number." It is a gateway converter, not an architectural primitive, and it will never
  be spoken of as a feature.
- **Interop honesty:** TARS supports neither MCP nor A2A today, and that is fine. We do
  not backfill compatibility badges to look complete. Absence is the truth; we state it.

## 10. What would change our mind

Because an honest ADR names its own falsifier. We would revisit the *server adapter*
(never the client) only if:

1. A closed host we care about becomes a **material distribution channel** for TARS's
   sandboxed execution and MCP is the only way in. A distribution trigger, not a
   technical one.
2. MCP grows a **real composition primitive** — typed tool-to-tool wiring with
   guarantees — and stops being a flat `Json → Text` bag. Then §5.3 no longer holds and
   the thing is worth re-reading on its merits.

Until one of those is true, the answer is a cheerful, well-evidenced **no**.

---

## Notes & cross-references

**Evidence, and how it was checked.** The two external claims this ADR leans on were
verified against primary sources: the
[OWASP MCP Top 10](https://owasp.org/www-project-mcp-top-10/)
([project repo](https://github.com/OWASP/www-project-mcp-top-10)) is a real OWASP
Foundation project, and
[CVE-2025-6514](https://nvd.nist.gov/vuln/detail/CVE-2025-6514) is a real NVD entry
(CVSS 9.6, package `mcp-remote`, OS command injection, reported by JFrog). We
deliberately do *not* lean on the loose vulnerability percentages that circulate in 2026
blog posts ("X% of servers vulnerable to Y"): they trace only to vendor syntheses whose
underlying surveys we could not open, and at least one widely-echoed figure turned out
to be a model extrapolation that bounced around the low-authority web until it looked
like a fact. If a credible first-party audit later lands, cite it here with the primary
link, not a blog echo.

**Internal.** Tool layer [Doc 23](23-unified-tool-layer.md) · sandbox / tenancy
[Doc 10](10-security-model.md), [Doc 29](29-agent-security.md) · typed decode
[Doc 15](15-output-validation.md) · Agent / Task [Doc 20](20-agent-abstraction.md) ·
market landscape [comparison.md](../comparison.md).
