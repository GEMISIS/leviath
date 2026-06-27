# Reviewer Agent

Automated code review agent that checks for quality, security, and best practices.

## Stages

1. **scan** - Quick pass for obvious issues (performance, security)
2. **deep_review** - Detailed analysis with Claude Opus
3. **report** - Generate structured review with recommendations

## Usage

```bash
lev spawn reviewer --task "Review PR #123"
```

## Context Layout

- **guidelines** (Pinned, 3K) - Code review guidelines and standards
- **diff** (Pinned, 20K) - The code changes being reviewed
- **findings** (Temporary, 25K) - Issues discovered during scan
- **analysis** (SlidingWindow, 20K) - Rolling analysis notes
- **report** (Clearable, 8K) - Final review report (wiped between reviews)

## Use Cases

- **PR reviews** - Automated first-pass review before human review
- **Security audits** - Focused scan for security issues
- **Architecture review** - Check adherence to design patterns
- **Style compliance** - Enforce coding standards

## Context Transform

Can receive code from the `coder` agent:
- `coder.implementation` → `reviewer.diff`
- `coder.architecture` → `reviewer.guidelines`
