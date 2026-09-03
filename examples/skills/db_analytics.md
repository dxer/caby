---
name: 数据库性能排查
description: 排查 postgres 慢查询、索引健康、表结构分析
keywords:
  - database
  - slow query
  - postgres
allowed_tools:
  - postgres:query
  - postgres:list_tables
---
# 执行准则
1. 任何写操作必须显式确认。
2. 慢查询先看执行计划再下结论。
3. 涉及生产环境时只读操作。
