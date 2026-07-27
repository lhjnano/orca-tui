# orcatui Feature Roadmap

> Orca GUI 기능 대비 orcatui의 현재 상태, 갭 분석, 단계적 구현 계획.
>
> **작성일**: 2026-07-27
> **orcatui 버전**: 0.3.1
> **Orca GUI 버전**: 1.4.156

---

## 1. 현재 상태 (orcatui v0.3.1)

### 구현 완료

| # | 기능 | 상태 | 비고 |
|---|------|:----:|------|
| 1 | 단일 에이전트 pane (PTY → vt100 → ratatui) | ✅ | |
| 2 | 멀티 pane 그리드 레이아웃 | ✅ | |
| 3 | 에이전트 레지스트리 (Claude/Codex/OpenCode/Gemini/Amp/Cursor) | ✅ | |
| 4 | AgentBus (tokio mpsc, N→1) | ✅ | |
| 5 | FrameScheduler (60fps throttle + frame-skip + idle backoff) | ✅ | |
| 6 | WorktreeManager (git worktree per agent) | ✅ | |
| 7 | 오케스트레이션 (sequential/parallel) | ✅ | CLI only |
| 8 | SSH remote (--remote, --reconnect) | ✅ | |
| 9 | GitHub/Linear (prs/issues via gh) | ✅ | CLI only |
| 10 | 모바일 동반자 (WebSocket server) | ✅ | |
| 11 | Orca 데몬 클라이언트 (--daemon) | ✅ | |
| 12 | 내장 데몬 서버 (daemon/attach) | ✅ | |
| 13 | Spawn picker (Ctrl+N 에이전트 선택) | ✅ | |
| 14 | Zoom 모드 (z), 도움말 (?), pane 종료 (x) | ✅ | |
| 15 | 사이드바 (에이전트 목록 + 상태 점 + 핀) | ✅ | |
| 16 | 점프 팔레트 (/), 토스트 알림 | ✅ | |
| 17 | 스마트 디폴트 (bare orcatui → 자동 감지/attach) | ✅ | |

### 미구현 (Orca GUI 대비)

Orca GUI의 8개 TopLevelView 중 orcatui는 `terminal`만 구현:
- `tasks` (이슈/PR 브라우저) — CLI만 있음
- `activity` (에이전트 활동 피드) — ❌
- `automations` (예약 실행) — ❌
- `settings` (설정 페이지) — config.toml만 있음
- `space` (워크스페이스 그룹) — ❌
- `skills` (스킬 관리) — ❌

---

## 2. Orca GUI 전체 기능 카탈로그

### 2.1 에이전트 관리 (Agent Management)

| 기능 | 설명 | TUI 적합성 |
|------|------|:----------:|
| **에이전트 카탈로그** | 30+ TUI 코딩 에이전트 레지스트리 | ✅ 데이터만 있음 |
| **에이전트 스폰/런치** | 바이너리 + args + env + 프롬프트 주입 | ✅ `src/shared/`에 순수 로직 |
| **상태 추적 엔진** | working/done/waiting/blocked/interrupted 상태 머신 | ✅ 타입이 `src/shared/`에 있음 |
| **Keep Awake** | 에이전트 작업 중 OS 슬립 방지 | ✅ `caffeinate`/`systemd-inhibit`로 대체 |
| **에이전트 최대절전** | 완료/유휴 에이전트 자동 종료 (메모리 절약) | ✅ 순수 로직 |
| **슬리핑 세션 캡처** | 종료된 에이전트의 세션 기록 → 재개 가능 | ✅ `src/shared/`에 스키마 |
| **슬리핑 에이전트 웨이크** | `--resume` 로 에이전트 복원 | ✅ 명령 빌더는 포터블 |
| **워크트리 슬립** | 워크트리의 모든 pane 종료 + 세션 보존 | ✅ 종료/캡처는 포터블 |
| **에이전트 훅 시스템** | 루프백 HTTP 서버로 에이전트 생명주기 이벤트 수신 | ✅ 명시적으로 헤드리스 설계 |
| **Claude 통합** | hooks.json + statusLine 스크립트 설치 | ✅ 파일 쓰기만 |
| **Codex 통합** | TOML config + app-server + 세션 관리 (89 파일) | ✅ 대부분 config/RPC |
| **에이전트 피커 UI** | cmdk 기반 검색 드롭다운 | ✅ 검색 로직은 포터블 |
| **에이전트 설정** | 에이전트별 명령 오버라이드, 권한, 런타임 | ✅ 설정 구조는 포터블 |
| **신뢰 프리셋** | Cursor/Copilot/Codex 폴더 신뢰 마커 사전 작성 | ✅ 순수 파일 시스템 |
| **활동 피드** | 에이전트 이벤트 알림 센터 (읽지 않음 배지, 인라인 미리보기) | ⚠️ 개념은 가능, 마크다운/임베디드 터미널은 불가 |
| **오케스트레이션/메시징** | 에이전트 간 작업 디스패치 (CLI 기반) | ✅ 이미 CLI-first |
| **모바일 에뮬레이터** | iOS/Android 기기 제어 (scrcpy/simctl) | ❌ 비디오 렌더링 필요 |

### 2.2 통합 (Integrations)

| 통합 | 깊이 | TUI 적합성 |
|------|------|:----------:|
| **GitHub** | PR 생성/머지/리뷰/체크, 이슈, Projects v2, rate-limit | ⚠️ mutation은 가능, Projects 보드는 어려움 |
| **GitLab** | MR 생성/머지/리뷰, 이슈, CI/CD | ⚠️ mutation은 가능 |
| **Linear** | 이슈/프로젝트/댓글/관계 (GraphQL) | ✅ 태스크 트래커 → TUI 적합 |
| **Jira** | 이슈/댓글 (Cloud + Server/DC) | ✅ HTTP 클라이언트 |
| **Azure DevOps** | PR 읽기/생성 | ✅ REST |
| **Bitbucket** | PR 읽기 | ✅ REST |
| **Gitea/Forgejo** | PR 읽기/생성 | ✅ REST |
| **Automations** | RRULE 스케줄, precheck, 헤드리스 디스패치, 사용량 추적 | ✅ 명시적으로 헤드리스 지원 |
| **Hermes** | Hermes 에이전트 Python 플러그인 훅 | ✅ 파일 시스템 |

### 2.3 워크스페이스 관리

| 기능 | 설명 | TUI 적합성 |
|------|------|:----------:|
| **워크트리 FS 추상화** | Windows/WSL 경로 처리, 삭제 재시도 | ✅ 순수 로직 |
| **워크트리 삭제 복구** | 부분 삭제 조정 (Git 등록 vs 디렉토리) | ✅ 순수 로직 |
| **대시보드** | 3-버킷 에이전트 보드 (도움 필요/작업중/유휴) | ✅ 스냅샷 계약이 UI 무관 |
| **사이드바** | 워크스페이스 카드, 정렬/필터, 드래그, 라인리지 | ⚠️ 정보 구조는 필수, DnD는 불가 |
| **칸반 보드** | 상태 레인 + 드래그 앤 드롭 | ❌ DnD는 TUI에 부적합 |
| **워크트리 라인리지** | 부모/자식 워크트리 관계 | ✅ 순수 로직 |
| **외부 워크트리 인박스** | 외부에서 생성된 워크트리 발견 | ✅ |
| **프로필 전환** | 다중 Orca config 프로필 | ✅ |
| **i18n** | en/es/ja/ko/zh (5개 언어) | ⚠️ JSON 카탈로그 재사용 가능 |

### 2.4 TopLevelView 라우트

Orca GUI의 8개 화면:

| View | 설명 | orcatui | TUI 우선순위 |
|------|------|:-------:|:----------:|
| `terminal` | 워크스페이스 + 터미널 스플릿 + 에이전트 | ✅ | — |
| `tasks` | GitHub/Linear/Jira 이슈 + PR 브라우저 | ❌ | **높음** |
| `activity` | 에이전트 활동 피드 + 알림 | ❌ | **높음** |
| `automations` | 예약/트리거 기반 에이전트 디스패치 | ❌ | 중간 |
| `settings` | 테마/레이아웃/에이전트/키바인딩 설정 | ❌ | **높음** |
| `space` | 워크스페이스 그룹 (폴더) | ❌ | 낮음 |
| `skills` | 스킬 마켓플레이스/관리 | ❌ | 낮음 |
| `mobile` | 모바일 페어링 | CLI | 낮음 |

---

## 3. 갭 분석: TUI에서 가치가 높은 기능

### 3.1 핵심 갭 (즉시 영향)

| 갭 | 현재 상태 | 목표 | 효과 |
|----|----------|------|------|
| **사이드바가 네비게이션이 아님** | 에이전트 목록만 표시 | Tasks/Agents/Settings 메뉴 추가 | 사이드바 → 네비게이션 허브 |
| **에이전트 상태가 단순함** | Running/Done/Failed 3상태 | working/waiting/blocked/interrupted 추적 | "도움이 필요한" 에이전트 즉시 파악 |
| **활동 피드 없음** | 상태 변화를 눈으로 확인 | 이벤트 로그 + 미확인 배지 | 백그라운드 에이전트 변화 놓치지 않음 |
| **이슈/PR이 CLI만** | `orcatui prs`, `orcatui issues` | 인터랙티브 뷰 + 에이전트 디스패치 | 이슈 선택 → 바로 에이전트 배정 |
| **설정이 파일 편집만** | config.toml 수동 편집 | 오버레이에서 실시간 토글 | 테마/레이아웃 즉시 변경 |

### 3.2 중요 갭 (다음 스텝)

| 갭 | 효과 |
|----|------|
| **에이전트 훅 시스템** | 에이전트의 PreToolUse/PostToolUse 이벤트 수신 → 정확한 상태 추적 |
| **에이전트 최대절전** | 완료된 에이전트 자동 종료 → 메모리 절약 |
| **슬리핑 세션** | 에이전트 종료 후에도 대화 맥락 보존 → `--resume` 복원 |
| **자동화** | "PR 열리면 에이전트 배정" 같은 트리거 |
| **대시보드** | 전체 에이전트를 상태별 그룹으로 한눈에 |

### 3.3 TUI에서 불필요/불가능

| 기능 | 이유 |
|------|------|
| 칸반 보드 (DnD) | 드래그 앤 드롭 불가 |
| 모바일 에뮬레이터 | 비디오 스트림 필요 |
| 마크다운 렌더링 | 터미널에서 제한적 |
| dock badge | 데스크탑 전용 |
| WebGL 터미널 | GPU 가 필요 |

---

## 4. 단계적 로드맵

### Phase 1: 사이드바 진화 + 상태 강화 (1-2주)

**목표**: 사이드바를 단순 목록에서 네비게이션 허브로 승격

#### 1.1 사이드바 상태 요약
- 현재 footer에만 있는 `● N working · ✗ N failed · ✓ N done`를 사이드바 하단에도 표시
- 에이전트 수가 적을 때 footer가 보이지 않는 경우 대비
- **난이도**: ⬜ 낮음 (30분)
- **파일**: `sidebar.rs`

#### 1.2 에이전트 상태 확장
- 현재: `Running` / `Done` / `Failed` / `Idle`
- 추가: `Waiting` (승인 대기), `Blocked` (차단), `Interrupted` (중단)
- OSC 9999 활동 데이터를 더 정밀하게 파싱해서 상태 매핑
- **난이도**: 🟨 중간 (반나절)
- **파일**: `agent.rs`, `osc.rs`, `sidebar.rs`

#### 1.3 사이드바 네비게이션 모드
- 사이드바 내에서 `↑↓`로 메뉴 항목 이동:
  - `▸ Tasks` — 이슈/PR 브라우저 열기
  - `▸ Agents` — 활동 피드 열기
  - `▸ Settings` — 설정 오버레이 열기
  - `───` 구분선
  - 에이전트 목록 (현재 기능)
- 새 `InputMode::Sidebar` 추가
- `Ctrl+S` 또는 사이드바 클릭으로 진입
- **난이도**: 🟨 중간 (1-2일)
- **파일**: `app.rs`, `sidebar.rs`

#### 1.4 에이전트 활동 타임라인
- 최근 N개 이벤트를 버퍼에 저장:
  - 상태 변화: `[12:03:45] claude: Running → Waiting (approval)`
  - 출력 마일스톤: `[12:04:01] codex: wrote 234 lines to src/main.rs`
  - 에러: `[12:04:15] opencode: Failed (exit code 1)`
- 전체 화면 오버레이로 표시 (Activity 뷰)
- **난이도**: 🟨 중간 (반나절)
- **파일**: `app.rs` (새 `ActivityLog` 구조체)

### Phase 2: 통합 뷰 (2-3주)

**목표**: CLI 전용 기능을 인터랙티브 뷰로

#### 2.1 Tasks 뷰 (전체 화면)
- `integrations.rs` 재사용
- GitHub/GitLab 이슈 + PR 목록을 스크롤 가능한 리스트로
- 항목 선택 → 에이전트 디스패치 (이슈 내용이 프롬프트로)
- `InputMode::Tasks` + 전체 화면 오버레이
- **난이도**: 🟨 중간 (1-2일)
- **파일**: `app.rs`, `integrations.rs`

#### 2.2 설정 오버레이
- `?` 도움말 패턴과 동일한 풀스크린 패널
- 토글: 테마 (GitHub-dark / custom), 사이드바 너비, 상태 바, 기본 에이전트
- 슬라이더: 스크롤백 라인 수
- 변경 즉시 `config.toml`에 저장
- **난이도**: 🟨 중간 (반나절)
- **파일**: `app.rs`, `config.rs`

#### 2.3 Agent Dashboard
- 전체 화면에서 에이전트를 3-버킷으로 분류:
  - `⚠ Needs Attention` — waiting/blocked/interrupted
  - `⚙ Working` — running
  - `✓ Done / ✗ Failed` — terminal
- Orca의 `DashboardSnapshot` 패턴 참고
- **난이도**: 🟨 중간 (반나절)
- **파일**: `app.rs`

### Phase 3: 에이전트 고도화 (3-4주)

**목표**: 에이전트 상태 추적 정확도 향상 + 세션 지속성

#### 3.1 에이전트 훅 시스템
- Orca의 `agent-hooks/` 포팅 (루프백 HTTP 서버)
- 에이전트별 hook 스크립트 설치/제거
- PreToolUse, PostToolUse, Stop 이벤트 수신
- 수신된 이벤트로 정확한 상태 추적 (working/waiting/done)
- **난이도**: 🔴 높음 (1주)
- **새 파일**: `hooks.rs`, `hook_server.rs`

#### 3.2 에이전트 최대절전
- 완료된 에이전트를 N분 후 자동 종료
- 메모리 절약 (20개 에이전트 실행 시 유효)
- 종료 전 세션 기록 저장
- **난이도**: 🟨 중간 (2-3일)
- **파일**: `app.rs`

#### 3.3 슬리핑 세션 + 웨이크
- 종료된 에이전트의 세션 메타데이터를 디스크에 저장
- `--resume` 플래그로 에이전트 복원
- 데몬 재시작 시 자동 복원
- **난이도**: 🔴 높음 (1주)
- **새 파일**: `session_store.rs`

#### 3.4 Keep Awake
- 에이전트 작업 중 OS 슬립 방지
- Linux: `systemd-inhibit` 또는 `caffeinate`
- macOS: `caffeinate`
- **난이도**: ⬜ 낮음 (반나절)
- **파일**: `app.rs`

### Phase 4: 자동화 + 워크플로우 (4-6주)

**목표**: 무인 에이전트 운영

#### 4.1 자동화 스케줄러
- cron 기반 예약 실행 (RRULE)
- precheck (쉘 명령으로 조건 확인)
- 헤드리스 디스패치 (데몬과 연동)
- 사용량 추적 (토큰/비용)
- **난이도**: 🔴 높음 (2주)
- **새 파일**: `automations.rs`

#### 4.2 오케스트레이션 UI
- 현재 `orchestrate` CLI를 사이드바에서 인터랙티브하게
- spec 입력 → 의존성 그래프 시각화 → 디스패치
- 에이전트 간 메시징 (orca의 `orchestration send/check`)
- **난이도**: 🔴 높음 (1-2주)
- **파일**: `app.rs`, `coordinator.rs`

#### 4.3 프로필 전환
- 다중 config 프로필 (작업/개인/실험)
- 사이드바에서 빠른 전환
- **난이도**: ⬜ 낮음 (반나절)
- **파일**: `config.rs`, `sidebar.rs`

### Phase 5: 선택적 확장 (필요시)

| 기능 | 난이도 | 비고 |
|------|:------:|------|
| 다중 통합 (Linear/Jira 직접 API) | 🔴 | 현재는 GitHub/GitLab만 |
| 워크트리 라인리지 | 🟨 | 부모/자식 관계 |
| 외부 워크트리 인박스 | 🟨 | 외부 생성 worktree 발견 |
| i18n (ko/ja/zh) | ⬜ | JSON 카탈로그 |
| 모바일 QR 페어링 | ⬜ | 터미널에 QR 표시 |

---

## 5. 기술 접근 방식

### 5.1 아키텍처 원칙

Orca GUI는 3계층 분리:
```
src/shared/    순수 TypeScript (전자/React 무관) ← TUI 포팅의 핵심
src/main/      Node/Electron 메인 프로세스
src/renderer/  React 프레젠테이션
```

orcatui는 이미 Rust로 같은 분리를 달성:
```
src/agent.rs       에이전트 타입/상태 (shared 대응)
src/app.rs         앱 로직 + 렌더링 (renderer 대응)
src/config.rs      설정 (shared 대응)
src/pty_session.rs PTY 관리 (main 대응)
```

### 5.2 우선순위 결정 기준

1. **사용 빈도**: 매일 쓰는 기능 > 가끔 쓰는 기능
2. **TUI 적합성**: 텍스트로 표현 가능 > 시각적 요소 필요
3. **구현 효율**: 기존 코드 재사용 > 새 인프라 필요
4. **의존성**: 외부 서비스 불필요 > API 인증 필요

### 5.3 Orca `src/shared/` 참고

로드맵 구현 시 Orca의 `src/shared/` (929 파일)를 참고:
- `agent-status-types.ts` — 상태 머신 정의
- `tui-agent-startup.ts` — 에이전트 런치 명령 빌더
- `dashboard-snapshot.ts` — 대시보드 데이터 계약
- `workspace-session-schema.ts` — 세션 지속성 포맷
- `agent-hook-listener.ts` — 훅 프로토콜

이들은 순수 TypeScript라 Rust로 포팅하기 쉬움.

---

## 6. 마일스톤 요약

| Phase | 기간 | 핵심 결과물 | 효과 |
|-------|------|------------|------|
| **1** | 1-2주 | 사이드바 네비게이션 + 상태 확장 + 활동 타임라인 | 에이전트 모니터링 강화 |
| **2** | 2-3주 | Tasks 뷰 + 설정 오버레이 + 대시보드 | CLI → 인터랙티브 |
| **3** | 3-4주 | 훅 시스템 + 최대절전 + 세션 지속성 + keep-awake | 정확한 상태 추적 + 메모리 효율 |
| **4** | 4-6주 | 자동화 + 오케스트레이션 UI + 프로필 | 무인 운영 |
| **5** | 필요시 | 다중 통합 + 라인리지 + i18n | 확장성 |

**Phase 1 완료 시 orcatui는 Orca GUI의 "TUI 에센셜"이 됨.**
