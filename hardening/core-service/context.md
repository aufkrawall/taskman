# Working Context: Core Service Boundary

This analysis was prepared in `C:/Users/REDACTED/Programme/build/tmproject` from
source review at Git revision `13696d08f17e03754cb842c74c52ceb59c06991a`
plus the in-progress GUI parity changes requested in the same development
session.

## Evidence inventory

The source-evidence collection digest is
`6588135e9f18606e014663a5fe5779209ae9d9575cbe4a7c48c19fba413a9b7a`.
It is SHA-256 over the repository-relative path and SHA-256 of each file below,
joined in the listed order.

| Evidence | Path | SHA-256 | Purpose |
| --- | --- | --- | --- |
| `E001` | `Cargo.toml` | `48e6df1bdccccb916006b0936ad93455da2d15cc53a4d124332d268730290ce3` | Workspace and release-profile boundary. |
| `E002` | `crates/tm-core/src/settings.rs` | `36d0d1951b482e0dde377bda8d542c0f470278cf65631a27e86d99f0eaa4e1bf` | Per-user configuration and persistence boundary. |
| `E003` | `crates/tm-platform/src/actions.rs` | `223e89023e00020763c6482f06b8d965026a6ae30dcd2caef53b57e7c3e89dc5` | Current platform action surface. |
| `E004` | `crates/tm-platform/src/win/mod.rs` | `3e0bf84392ac9d8bf2baebf62c800b025faa8d29cfb4102423487661e13eca3e` | Current Windows action dispatch and elevation helpers. |
| `E005` | `crates/tm-app/src/main.rs` | `e8a3bd9b1b6a4e5eca6b4d4c9651bc3885e5c1d6baf5670bec6f98ea2a13e402` | GUI process startup/elevation and renderer selection. |
| `E006` | `crates/tm-app/src/app.rs` | `a523cc8e293063daa5b8006da89d5dfab6010a0c9ae00831683d1c2138f76535` | GUI engine/actions ownership and worker behavior. |

## External design references

Microsoft's Windows documentation was consulted for named-pipe DACLs and
client impersonation, named-pipe client PID attribution, service SIDs,
service object security, service failure actions, delayed automatic start,
and required service privileges. These references inform the proposed design;
they are not evidence that the implementation already satisfies it.

## Epistemic labels

- **Observed** means directly present in the inventoried source.
- **Inferred** means a structural consequence of the observed source and the
  Windows security model.
- **Proposed** behavior appears only in option and implementation sections.

## Drift note

The repository had in-progress task-owned edits when the evidence collection
was recorded, so `sourceDrift` is `present`. The implementation plan requires
rechecking every affected boundary against the final diff before declaring the
selected design complete.

## Final implementation evidence

The selected design was implemented and committed as
`fefaeb9376428128f7bee952f957c10559eb9813`. The final source collection below
has digest
`5803bebd8726401a016092fdb3846f2b75866d9058885e60ffae87f1fc4141d6`.
It uses the same sorted `path<TAB>sha256<LF>` construction as the baseline
collection. The baseline drift is therefore resolved for the implementation
claims recorded here; the original collection remains unchanged as historical
architecture evidence.

| Evidence | Path | SHA-256 | Purpose |
| --- | --- | --- | --- |
| `E101` | `build.py` | `25d2e8c1beb5541d18ef7348fb06c36a78cc0253c0d1f6366a0b4d0bffa4dfca` | Release packaging includes both protected executables. |
| `E102` | `Cargo.toml` | `9ba6d6b2f8c87f78f0539c3e3c2d7e5fc50c35d6b823bd9de8b8b787f47b30d3` | Workspace membership and release profile. |
| `E103` | `crates/tm-app/src/app_ui.rs` | `438787837d3fb72714c8a9b631be67b42e08c9c4de0c04f9a881a9181020b0e9` | Service lifecycle and reliability controls in the GUI. |
| `E104` | `crates/tm-app/src/app.rs` | `ed073e93400417af58ab877497440b8f14d23fc1421c26282feb1de4c725c917` | Bounded action lanes, broker selection, and state ownership. |
| `E105` | `crates/tm-app/src/main.rs` | `802debbe2cf6ff372b69acb33654acf25f719493509298068ab706499707b42a` | Single-instance, tray, elevation handoff, and renderer startup. |
| `E106` | `crates/tm-app/src/tabs/details.rs` | `9a530926572090183b4f45ab413ca480b78f060e473f93fd08bfef5247f7dac1` | Details tree and protected process controls. |
| `E107` | `crates/tm-app/src/tabs/modules.rs` | `da56b7708fc9908ed4a1f7fb3480de84a2903ee28bd9985a63e5ac189c3eaa01` | On-demand module inventory and guarded unload workflow. |
| `E108` | `crates/tm-app/src/tabs/processes.rs` | `0adeab80e46cd2dfae8c79913ffb920ede69d394ca87b04999c7b6b1c867273a` | Process status and persisted presentation state. |
| `E109` | `crates/tm-core/src/logging.rs` | `3b4a246d9f3861015cc8252f5acdc58cfd3ff5bc2da0670f603e7ff543facf3c` | Protected service log lifecycle. |
| `E110` | `crates/tm-core/src/settings.rs` | `029df28e59f4299cccd94ac6b581c3a5f2e73c91e3431faf3ed2b4ce956955d9` | Bounded configuration and persisted policies. |
| `E111` | `crates/tm-platform/src/actions.rs` | `fc7bffa383ebf0dfbd8d5c2ee6869c16563c163881832ae0837070c61041db09` | Semantic action boundary shared by GUI and broker. |
| `E112` | `crates/tm-platform/src/win/autostart.rs` | `7f3065fc87bb188ae5e0479eb31579fec07e65aeb2ed7d7b399aa57bd94551d4` | Owned per-user autostart mutation. |
| `E113` | `crates/tm-platform/src/win/core_service.rs` | `6a353ce8f89234906195332d48f2f4cf1c65009a5915dc70be95e9c4541437ce` | Protocol, authentication, install, ACL, and SCM boundary. |
| `E114` | `crates/tm-platform/src/win/process_ops.rs` | `cf1fbe49f2b4fcb98282b40ccc17fc0f485422c537b59cf655be8664e7c13343` | Exact-identity process and module mutations. |
| `E115` | `crates/tm-platform/src/win/taskmgr_replacement.rs` | `12d8052586fba99070fb10fca61bd6b782bff48d1e7aff70ffc9cce946b65aac` | Owned Task Manager replacement registration. |
| `E116` | `crates/tm-service/src/main.rs` | `0b31eeb0d10cf05b6e54165ec3b9b79fe4e676fec7bf2b1f42ccc176fbffe2f9` | LocalSystem service lifecycle, authorization, and bounded workers. |
