---
name: PR 代码审查与质量检查
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
keywords:
  - code review
  - pull request
  - 代码审查
allowed_tools:
  - github:get_pull_request
  - github:create_review_comment
---
# 执行准则与安全规范
1. 必须先通过 `get_pull_request` 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
