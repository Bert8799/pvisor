# Upstream provenance and local changes

- Project: Podman, https://github.com/podman-container-tools/podman
  (formerly https://github.com/containers/podman).
- Revision: `3279be7b55b1b91741e61147b84f8ce3971c1c3d`.
- Source: https://github.com/podman-container-tools/podman/tree/3279be7b55b1b91741e61147b84f8ce3971c1c3d/test
- Imported: 2026-10-21.
- License: Apache-2.0; verbatim upstream license at `upstream/LICENSE`.
  Individual upstream copyright/license notices remain intact.
- `upstream/test/system/` and `upstream/test/e2e/` are complete regular-file
  copies from that revision, including fixtures. `upstream/test/README.md`
  is included as context. No upstream `NOTICE` file existed in this revision.
- `upstream-sha256.json` records SHA-256 of each imported file. The normal
  Python regression test checks file contents and inventory against it.

Local adaptations (Apache-2.0, pVisor contributors):

| Active file/case | Upstream source | Adaptation |
| --- | --- | --- |
| `system/helpers.bash`, `command.py` | `test/system/helpers.bash` | pVisor-only dispatch, private store, exact-name cleanup, 120-second timeout; no Podman environment setup/reset |
| `system/030-run.bats`: exit, missing executable, stdin, env/workdir | `test/system/030-run.bats` | Select common Docker CLI options, fixed Alpine workload, bounded execution |
| `system/030-run.bats`: bind mount, read-only mount | `test/system/160-volumes.bats` | No volume-management commands or SELinux relabel option; verify user-visible files |
| `e2e/cli_test.go`: `TestRunStreamsAndExit` | `test/e2e/run_test.go` | Separate stream assertions and workload exit status, using standard Go testing |
| `e2e/cli_test.go`: `TestExecStdinAndExit` | `test/e2e/exec_test.go` | Check stdin/EOF, workload exit and missing executable without inspect |
| `e2e/cli_test.go`: `TestStopStartPersistsFiles` | `test/e2e/start_test.go`, `stop_test.go` | Observe file persistence through exec instead of Podman state structs |
| `e2e/cli_test.go`: `TestAutomaticRemoval` | `test/e2e/rm_test.go`, `run_test.go` | Verify name reuse after foreground `--rm` |
| `e2e/cli_test.go`: `TestDuplicateNameFails` | `test/e2e/run_test.go` | Verify duplicate-name failure preserves the first container |
| `system/031-run-options.bats`: env-file/host env | `test/e2e/run_env_test.go` | Use explicit values and file precedence without Podman host-env options |
| `system/031-run-options.bats`: entrypoint/user/workdir | `test/system/030-run.bats`, `test/e2e/run_test.go` | Single executable entrypoint, literal argv, Alpine numeric/named users and filesystem observation |
| `system/031-run-options.bats`: bind workdir/read-only delete | `test/system/160-volumes.bats` | Portable directory binds; retain host-file content assertion |
| `e2e/cli_test.go`: `TestExecEnvironmentAndWorkdir`, `TestExecUserAndStreams` | `test/e2e/exec_test.go` | Per-exec overrides must not change subsequent exec defaults; separate stream assertions |
| `e2e/cli_test.go`: `TestExecLargeStdin` | `test/system/075-exec.bats` | Compare all 1.7 MB byte-for-byte instead of running host checksum tools |
| `e2e/cli_test.go`: `TestPsLifecycleAndIDSelection` | `test/system/040-ps.bats`, `test/e2e/ps_test.go` | Observe exact own ID membership via quiet listing and short-ID selection; no formatting/filters |
| `e2e/cli_test.go`: `TestLogsPreserveStreamsAfterExit` | `test/system/035-logs.bats`, `test/e2e/logs_test.go` | Foreground run replaces create/start-attach/wait; repeated logs must preserve both streams |
| `e2e/cli_test.go`: `TestImagesAndRmi` | `test/e2e/images_test.go`, `test/e2e/rmi_test.go` | Assert full/short ID and reference removal within a private pVisor store |
| `e2e/cli_test.go`: `TestNamedVolumeSurvivesContainerRmV` | `test/system/160-volumes.bats` | Verify named volume survives rm -v using a second container, no volume-inspect API |
| `e2e/cli_test.go`: `TestNamedVolumeCopiesImageContent` | `test/e2e/run_volume_test.go` | Compare Alpine image file with automatic volume copy-up and initialized read-only remount |
| `e2e/cli_test.go`: `TestRmRunningContainerRequiresForce` | `test/system/055-rm.bats`, `test/e2e/rm_test.go` | Observe nonzero rejection, live exec, forced removal and subsequent name reuse |
| `system/900-unsupported.bats.disabled`, `e2e/unsupported_test.go.disabled` | Sources recorded beside each commented command | Explicit exclusions with reasons; not executable tests or passing coverage |

The active files are ports of selected scenarios, not verbatim copies or a
claim that the entire upstream suites run on pVisor. The active cases run only
`pvisor docker` and check expected behavior, without a Docker comparison.
Explicitly disabled commands/options and Docker-daemon interoperation cases
are commented in the two
`.disabled` files; they do not produce skips or passes. Every other upstream case
without an explicit port remains unported and unvalidated, not automatically
unsupported. Future ports should update this table and keep upstream copies
unchanged. Runtime failures of active cases must remain failures.

Docker dependency exclusions:

- Upstream `test/e2e/push_test.go`, "podman push to docker daemon", starts/checks
  Docker and invokes `docker images`/`docker rmi`. It is explicitly disabled in
  `e2e/unsupported_test.go.disabled`; pVisor also lacks the push transport.
- The former local Docker engine selector, fixture tagging and Docker volume
  cleanup have been removed. Image/volume contracts still run in private pVisor
  stores; they need no Docker daemon and are not skipped.
- The pinned upstream copies remain verbatim reference material. Their original
  runners, including any Docker-dependent cases, are not invoked by this profile.
