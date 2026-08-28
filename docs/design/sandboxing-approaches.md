# Sandboxing Approaches for Production Agent Runtimes (2026)

## Executive Summary

The 2026 industry consensus is unequivocal: **shared-kernel containers are not a security boundary for AI agent code execution** [[7]](https://emirb.github.io/blog/microvm-2026/). The minimum viable sandbox for production agents executing arbitrary, untrusted code is a **microVM (Firecracker or Kata-backed)** providing hardware-level isolation [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox). gVisor serves as an acceptable fallback for medium-threat models, while V8 Isolates are appropriate only for JavaScript/WebAssembly-specific workloads. By early 2026, major platforms including Cloudflare, Vercel, Ramp, and Modal had all shipped dedicated sandbox features, with E2B, Northflank, and Firecrawl building entire infrastructure platforms around the problem [[12]](https://www.firecrawl.dev/blog/ai-agent-sandbox).

**Confidence: High** (Consistent across official vendor docs, Kubernetes SIG documentation, and independent security analyses)

---

## 1. Firecracker MicroVMs — Hardware-Level Isolation

Firecracker, developed by AWS and written in Rust, is the gold standard for agent sandbox isolation. It uses KVM (Kernel-based Virtual Machine) to provide **hardware-enforced isolation** — each workload runs in its own lightweight VM with a dedicated Linux kernel, completely separate from the host [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[7]](https://emirb.github.io/blog/microvm-2026/). 

**Key Specifications:**
* **Boot time:** ≤125ms to `/sbin/init` (serial console disabled), ~100-200ms depending on configuration [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)
* **Memory overhead:** ≤5 MiB per microVM [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)
* **Creation rate:** Up to 150 microVMs/second/host [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)
* **Codebase:** ~50K lines of Rust vs. QEMU's ~2M lines of C, eliminating entire classes of memory-safety vulnerabilities [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)
* **Attack surface:** Only 5-6 emulated virtio devices (network, block, vsock, balloon, serial, keyboard), minimizing the exploitable interface [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[7]](https://emirb.github.io/blog/microvm-2026/)

**Production Usage:** AWS Lambda and Fargate run on Firecracker directly [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[7]](https://emirb.github.io/blog/microvm-2026/). E2B uses Firecracker as its core isolation layer [[6]](https://northflank.com/blog/e2b-vs-modal) [[8]](https://github.com/e2b-dev/e2b) [[9]](https://e2b.dev/docs). Vercel Sandbox also runs on Firecracker microVMs [[10]](https://vercel.com/docs/sandbox/concepts).

**Limitations:** Firecracker does not provide built-in, officially supported GPU passthrough; upstream PCIe support excludes VFIO-based device passthrough, though experimental work exists [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[13]](https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/). It requires significant orchestration infrastructure to run at scale; most teams use it through Kata Containers or a managed platform [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[7]](https://emirb.github.io/blog/microvm-2026/).

**Snapshot Performance:** Firecracker snapshots enable restore in ~28ms, making per-request isolation viable for high-throughput agent loops [[17]](https://dev.to/adwitiya/how-i-built-sandboxes-that-boot-in-28ms-using-firecracker-snapshots-i0k) [confidence: medium — single blog source].

---

## 2. gVisor — User-Space Kernel Interception

gVisor, developed by Google, implements a **user-space kernel** (the Sentry, written in Go) that intercepts application system calls and handles them in user space rather than passing them to the host kernel [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/).

**Security Model:** Escape requires a bug in gVisor's Sentry AND a bug in the host kernel's handling of the Sentry's permitted syscalls — two independent codebases must be compromised [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox). However, gVisor does **not** provide hardware-level isolation [[18]](https://edera.dev/stories/kata-vs-firecracker-vs-gvisor-isolation-compared) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/).

**Key Characteristics:**
* **Syscall overhead:** 10-30% slower than native containers for I/O-heavy workloads [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor); one source reports 20-50% [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/) [confidence: medium — ranges differ between sources].
* **Syscall coverage:** Implements ~70-80% of Linux syscalls; advanced ioctl and eBPF may fail [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/).
* **GPU support:** Supports GPU workloads through nvproxy with negligible overhead for ML inference [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[13]](https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/).
* **Operational complexity:** Drop-in replacement for runc via Kubernetes RuntimeClass — low overhead [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/) [[19]](https://agent-sandbox.sigs.k8s.io/docs/use-cases/gvisor-isolation/).
* **Vulnerabilities:** gVisor itself has had security vulnerabilities in the Sentry [[22]](https://www.shayon.dev/post/2026/52/lets-discuss-sandbox-isolation/).
* **Multi-tenancy:** Does not automatically provide multi-job isolation within a single sandbox; additional controls are needed [[22]](https://www.shayon.dev/post/2026/52/lets-discuss-sandbox-isolation/).

**Production Usage:** Modal uses gVisor for sandbox isolation [[6]](https://northflank.com/blog/e2b-vs-modal) [[11]](https://manveerc.substack.com/p/ai-agent-sandboxing-guide) [[35]](https://www.amplifypartners.com/blog-posts/behind-the-scenes-of-modal-sandboxes) [[36]](https://modal.com/resources/best-gpu-enabled-sandboxes-ai-agents). Modal reports handling 250,000 applications in a single weekend with 20,000 concurrent sandboxes at peak [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [confidence: medium — reported by Augment Code citing Modal].

---

## 3. Container-Based Isolation — Insufficient for Untrusted Code

Standard Docker containers share the host kernel via Linux namespaces and cgroups [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/) [[7]](https://emirb.github.io/blog/microvm-2026/). **The Linux kernel has ~40 million lines of C and exposes 450+ syscalls** — this is the attack surface [[7]](https://emirb.github.io/blog/microvm-2026/).

**Container escapes are real and frequent.** Documented CVEs include: CVE-2019-5736 (runc escape), CVE-2024-21626 (Leaky Vessels), CVE-2024-1753 (Buildah/Podman), CVE-2025-9074 (Docker Desktop), CVE-2025-23266 (NVIDIA container toolkit), CVE-2025-31133 (runc masked path race), CVE-2025-52565 (runc /dev/console mount), CVE-2025-38617 (Linux kernel packet socket UAF) [[7]](https://emirb.github.io/blog/microvm-2026/).

Security profiles (seccomp, AppArmor, SELinux, capabilities) reduce but do not eliminate kernel escape risk [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/).

**Consensus:** Containers are acceptable for low-threat scenarios (internal tools, trusted code, non-production) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/) [[21]](https://adamtheautomator.com/containers-vs-gvisor-vs-microvms-azure-ai-agent/). For hostile or user-supplied AI-generated code, they are **explicitly insufficient** [[11]](https://manveerc.substack.com/p/ai-agent-sandboxing-guide) [[12]](https://www.firecrawl.dev/blog/ai-agent-sandbox) [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox).

---

## 4. V8 Isolates — Language-Level Sandboxing

V8 Isolates are sandboxed execution contexts within a single V8 engine instance. Each isolate has its own heap and cannot access other isolates' memory [[2]](https://developers.cloudflare.com/workers/reference/security-model/) [[24]](https://www.kunalganglani.com/blog/cloudflare-workers-v8-isolates-ai-agents) [[25]](https://fordelstudios.com/research/how-v8-isolates-actually-work-under-the-hood) [[27]](https://www.clodo.dev/blog/v8-isolates-comprehensive-guide).

**Cloudflare Workers** is the canonical production implementation. Key security properties [[2]](https://developers.cloudflare.com/workers/reference/security-model/):
* `Date.now()` is locked during execution; no timers or multi-threading allowed (prevents timing side-channel attacks)
* Dynamic process isolation for Workers needing extra isolation (e.g., debugger access)
* Each isolate receives a random key protecting V8 heap data, blocking cross-isolate reads in 92% of cases [[26]](https://blog.cloudflare.com/safe-in-the-sandbox-security-hardening-for-cloudflare-workers/)
* `workerd` can be configured with seccomp-bpf for additional syscall filtering [[30]](https://www.federicocalo.dev/en/blog/01-v8-isolates-explained-how-cloudflare-workers-eliminate-cold-starts)

**Limitations:** V8 Isolates only sandbox JavaScript/WebAssembly. They cannot execute arbitrary Python, shell scripts, or binary code. V8 has more bugs reported against it than VMs, requiring additional defense-in-depth layers [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[2]](https://developers.cloudflare.com/workers/reference/security-model/). V8 Isolates are also used by Deno Deploy, Fastly Compute@Edge, and Vercel Serverless Functions [[29]](https://dzx.cz/2023-03-08/how_do_cloudflare_workers_work/).

---

## 5. Kata Containers & WebAssembly

**Kata Containers** is **not itself an isolation technology** — it is an orchestration framework that integrates VMMs (Firecracker, Cloud Hypervisor, QEMU) with Kubernetes [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[47]](https://opencomputer.dev/guides/firecracker-vs-cloud-hypervisor-vs-kata/). It provides hardware-level isolation via its VMM backend while maintaining OCI-compatible container workflows [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/). Boot time: ~150-300ms depending on VMM and configuration [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[50]](https://onidel.com/blog/gvisor-kata-firecracker-2025). Kata abstracts the operational complexity of managing microVMs at scale [[18]](https://edera.dev/stories/kata-vs-firecracker-vs-gvisor-isolation-compared) [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor).

**WebAssembly** provides memory isolation through bounds-checked linear memory with WASI's capability model denying filesystem, network, and OS access by default [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/). Cold start is microseconds; runtime performance within 10% of native via AOT [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/). **Limitations for AI agents:** No persistent filesystem, limited syscall support, requires application rewrite [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/). Wasmtime is implementing CFI mechanisms to reduce Cranelift compiler bug impact [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox).

---

## 6. Production Platforms Compared

| Platform | Isolation | Boot Time | GPU | Persistence | Notes |
|----------|-----------|-----------|-----|-------------|-------|
| **E2B** | Firecracker microVM | ~150ms (snapshot pools) | CPU-focused (experimental GPU) | Session-scoped (up to 24h) | Open-source, SDK-defined templates [[6]](https://northflank.com/blog/e2b-vs-modal) [[8]](https://github.com/e2b-dev/e2b) [[9]](https://e2b.dev/docs) |
| **Modal** | gVisor | <1 second | Full GPU support | Session-scoped, snapshots | Full ML platform (inference, training, sandboxes) [[6]](https://northflank.com/blog/e2b-vs-modal) [[35]](https://www.amplifypartners.com/blog-posts/behind-the-scenes-of-modal-sandboxes) [[36]](https://modal.com/resources/best-gpu-enabled-sandboxes-ai-agents) |
| **Vercel Sandbox** | Firecracker microVM | Milliseconds | No GPU | Persistent by default, auto-snapshot | Single region (iad1), GA Jan 2026, $1M bounty [[10]](https://vercel.com/docs/sandbox) [[41]](https://northflank.com/blog/can-you-run-ai-agents-on-vercel) [[40]](https://www.techzine.eu/news/security/143695/vercel-offers-1-million-for-hacking-its-ai-sandbox/) |
| **Northflank** | Kata/Firecracker/gVisor | Sub-second | Full GPU | Ephemeral + persistent | BYOC, full workload runtime [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) [[6]](https://northflank.com/blog/e2b-vs-modal) |

---

## 7. Industry Consensus: Minimum Viable Sandboxing for AI Agents

The consensus across multiple sources establishes a **tiered threat model** [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/):

| Threat Level | Scenario | Minimum Isolation |
|-------------|----------|-------------------|
| **High** | AI agents executing LLM-generated code, financial/healthcare data, HIPAA/PCI-DSS | Firecracker or Kata microVM |
| **Medium** | Multi-tenant SaaS, cost-sensitive deployments, Kubernetes integration | gVisor |
| **Low** | Internal tools, trusted code, non-production | Standard containers |

**Key evidence for this consensus:**
* OWASP AIVSS assigns CVSS v4.0 Base Score of 9.4 to interpreter tool attacks where LLM agents execute attacker-provided code [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox)
* Palo Alto Unit 42 research demonstrated ChatGPT-4o as autonomous agent executing SQL injection, SSRF, and data exfiltration that its chat-only counterpart refused [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox)
* Multiple 2024-2025 CVEs demonstrated real container escape paths [[7]](https://emirb.github.io/blog/microvm-2026/)
* "The minimum acceptable isolation for a production agent execution sandbox is typically a Firecracker/Kata microVM, with gVisor used in some environments as a fallback" [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox)

---

## Contradictions & Open Questions

1. **gVisor syscall overhead:** Sources report 10-30% [[4]](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor) vs. 20-50% [[5]](https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/) slowdown for I/O-heavy workloads. The range is configuration-dependent and workload-specific.
2. **Firecracker GPU passthrough:** One source reports Firecracker's hardware virtualization path supports VFIO device passthrough for GPU access [[13]](https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/), but the Augment Code guide states upstream Firecracker has no native GPU passthrough, with only experimental PCI/vfio work [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox). The truth: experimental work exists but is not production-ready.
3. **gVisor GPU support:** gVisor supports GPU through nvproxy [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox) [[13]](https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/), but one source states gVisor's user-space kernel intercepts GPU calls at a point that blocks direct PCIe passthrough [[13]](https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/). The nuance: nvproxy handles NVIDIA GPU ioctls, but direct PCIe passthrough is blocked — gVisor can use GPUs but not with full bare-metal passthrough.
4. **Open question:** What is the actual escape complexity difference between Firecracker microVMs and gVisor in practice? While microVM escapes require hypervisor CVEs ($250K-$500K bounty class) [[7]](https://emirb.github.io/blog/microvm-2026/), gVisor escapes require both a Sentry bug AND a host kernel bug [[3]](https://www.augmentcode.com/guides/agent-execution-sandbox). Real-world comparative exploit data is scarce.
5. **Open question:** How do SmolVM, OpenSandbox, and other newer microVM platforms (SmolVM launched April 2026 [[15]](https://particula.tech/blog/smolvm-vs-firecracker-sandbox-ai-generated-code)) compare to Firecracker in production maturity? Insufficient data for a definitive assessment.

---

## Sources

[1] Vercel Sandbox docs — https://vercel.com/docs/sandbox
[2] Cloudflare Workers security model — https://developers.cloudflare.com/workers/reference/security-model/
[3] What Is an Agent Execution Sandbox? | Augment Code — https://www.augmentcode.com/guides/agent-execution-sandbox
[4] Kata Containers vs Firecracker vs gVisor | Northflank — https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor
[5] Firecracker, gVisor, Containers, and WebAssembly — SoftwareSeni — https://www.softwareseni.com/firecracker-gvisor-containers-and-webassembly-comparing-isolation-technologies-for-ai-agents/
[6] E2B vs Modal | Northflank — https://northflank.com/blog/e2b-vs-modal
[7] Your Container Is Not a Sandbox: The State of MicroVM Isolation in 2026 | Emir Beganović — https://emirb.github.io/blog/microvm-2026/
[8] E2B GitHub repo — https://github.com/e2b-dev/e2b
[9] E2B Documentation — https://e2b.dev/docs
[10] Vercel Sandbox concepts — https://vercel.com/docs/sandbox/concepts
[11] How to sandbox AI agents in 2026 | Manveer Chawla — https://manveerc.substack.com/p/ai-agent-sandboxing-guide
[12] AI Agent Sandbox: How to Safely Run Autonomous Agents in 2026 | Firecrawl — https://www.firecrawl.dev/blog/ai-agent-sandbox
[13] AI Agent Code Execution Sandboxes on GPU Cloud | Spheron Blog — https://www.spheron.network/blog/ai-agent-code-execution-sandbox-e2b-daytona-firecracker/
[14] How Firecracker microVMs work under the hood | Kerkour — https://kerkour.com/firecracker-sandboxing-rust
[15] SmolVM Explained | Particula — https://particula.tech/blog/smolvm-vs-firecracker-sandbox-ai-generated-code
[16] How to Sandbox AI Agent Code | Reinvently — https://reinvently.co.uk/blog/microvm-sandbox-options-firecracker-opensandbox-smolvm-nono/
[17] Firecracker snapshots 28ms boot | DEV Community — https://dev.to/adwitiya/how-i-built-sandboxes-that-boot-in-28ms-using-firecracker-snapshots-i0k
[18] Kata, gVisor, or Firecracker? | Edera — https://edera.dev/stories/kata-vs-firecracker-vs-gvisor-isolation-compared
[19] gVisor Isolation | Agent Sandbox - Kubernetes — https://agent-sandbox.sigs.k8s.io/docs/use-cases/gvisor-isolation/
[20] r/programming Sandboxes technical breakdown | Reddit — https://www.reddit.com/r/programming/comments/1q69bxn/sandboxes_a_technical_breakdown_of_containers/
[21] Containers vs. gVisor vs. MicroVMs for Azure AI Agent Security | Adam the Automator — https://adamtheautomator.com/containers-vs-gvisor-vs-microvms-azure-ai-agent/
[22] Let's discuss sandbox isolation | Shayon Mukherjee — https://www.shayon.dev/post/2026/52/lets-discuss-sandbox-isolation/
[23] Container Runtime Security | Systems Hardening — https://www.systemshardening.com/articles/linux/linux-container-runtime-alternatives/
[24] V8 Isolates: Why AI Agents Run 100x Faster | Kunal Ganglani — https://www.kunalganglani.com/blog/cloudflare-workers-v8-isolates-ai-agents
[25] How V8 Isolates Work | Fordel Studios — https://fordelstudios.com/research/how-v8-isolates-actually-work-under-the-hood
[26] Safe in the sandbox: Cloudflare Blog — https://blog.cloudflare.com/safe-in-the-sandbox-security-hardening-for-cloudflare-workers/
[27] V8 Isolates: From Concept to Production | Clodo — https://www.clodo.dev/blog/v8-isolates-comprehensive-guide
[28] Understanding Deno Workers | Medium — https://nut-charoenpattanasirikul.medium.com/understanding-deno-workers-v8-isolation-message-passing-and-the-permission-sandbox-0d856f26b2e3
[29] How do CloudFlare Workers work? | Matouš Dzivjak — https://dzx.cz/2023-03-08/how_do_cloudflare_workers_work/
[30] V8 Isolates Explained | Federico Calò — https://www.federicocalo.dev/en/blog/01-v8-isolates-explained-how-cloudflare-workers-eliminate-cold-starts
[31] Best Code Execution Sandboxes for AI Agents 2026 | Blaxel — https://blaxel.ai/blog/code-execution-sandboxes-for-ai-agents
[32] E2B Sandbox: Secure Code Execution for AI Agents | Effloow — https://effloow.com/articles/e2b-sandbox-secure-code-execution-ai-agents-guide-2026
[33] E2B Review | Infragap — https://infragap.com/tools/e2b/
[34] Mastering Secure AI Code Execution | Skywork — https://skywork.ai/skypage/en/Mastering-Secure-AI-Code-Execution-A-Deep-Dive-into-the-E2B-MCP-Server/1972499844382523392
[35] Behind the scenes of Modal sandboxes | Amplify Partners — https://www.amplifypartners.com/blog-posts/behind-the-scenes-of-modal-sandboxes
[36] Best GPU-Enabled Sandboxes for AI Agents | Modal — https://modal.com/resources/best-gpu-enabled-sandboxes-ai-agents
[37] Modal: specs, pricing & alternatives | CompareSandboxes — https://comparesandboxes.com/sandbox/modal/
[38] Modal | Ry Walker Research — https://rywalker.com/research/modal
[39] AI Agent Sandboxes Compared | Ry Walker Research — https://rywalker.com/research/ai-agent-sandboxes
[40] Vercel offers $1 million for hacking its AI sandbox | Techzine — https://www.techzine.eu/news/security/143695/vercel-offers-1-million-for-hacking-its-ai-sandbox/
[41] Can you run AI agents on Vercel? | Northflank — https://northflank.com/blog/can-you-run-ai-agents-on-vercel
[42] Run Python code securely with AI SDK and Vercel Sandbox | Vercel KB — https://vercel.com/kb/guide/python-ai-sdk-vercel-sandbox
[43] Vercel Sandbox in the Main CLI | Open Techstack — https://open-techstack.com/blog/vercel-sandbox-vercel-cli-2026/
[44] Where Should Your AI Agent Run Code | Developers Digest — https://www.developersdigest.tech/blog/ai-agent-code-sandbox-comparison-2026
[45] 5 Vercel Sandbox Alternatives | Blaxel — https://blaxel.ai/blog/vercel-sandbox-alternatives
[46] Container-to-VM Runtimes Compared | Ry Walker Research — https://rywalker.com/research/container-vm-runtimes
[47] Firecracker vs Cloud Hypervisor vs Kata Containers | OpenComputer — https://opencomputer.dev/guides/firecracker-vs-cloud-hypervisor-vs-kata/
[48] Best Firecracker Alternatives in 2026 | PandaStack — https://www.pandastack.ai/blog/best-firecracker-alternatives-2026/
[49] Firecracker vs Docker | HuggingFace blog — https://huggingface.co/blog/agentbox-master/firecracker-vs-docker-tech-boundary
[50] gVisor vs Kata Containers vs Firecracker | Onidel — https://onidel.com/blog/gvisor-kata-firecracker-2025