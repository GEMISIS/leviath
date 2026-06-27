# Researcher Agent

A multi-stage research assistant that gathers information, analyzes it, and produces concise summaries.

## Stages

1. **gather** - Collect relevant information on the topic
2. **analyze** - Deep analysis and synthesis of findings  
3. **summarize** - Create actionable summary

## Usage

```bash
lev spawn researcher --task "Research the impact of AI on software development"
```

## Context Layout

- **query** (Pinned, 1K tokens) - Original research question
- **findings** (Temporary, 40K tokens) - Raw research data
- **analysis** (Compacting, 50K tokens) - Analysis workspace with auto-compaction
- **summary** (CompactHistory, 10K tokens) - Distilled insights
- **scratch** (Clearable, 5K tokens) - Temporary working memory
