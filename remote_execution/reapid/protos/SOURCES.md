# Vendored proto sources

`reapid` speaks [Bazel's Remote Execution API v2](https://github.com/bazelbuild/remote-apis)
plus the [ByteStream API](https://github.com/googleapis/googleapis/blob/master/google/bytestream/bytestream.proto).
The `.proto` files below are vendored verbatim (import closure, no edits) so
`remote_execution/reapid/BUCK` can codegen them hermetically with the repo's pinned protoc.

## Pins

| Source | Pin |
| --- | --- |
| `bazelbuild/remote-apis` | `6def1c5d27a527c400875c24ae8b1a160145d7e1` (main, 2026-10-15) |
| `googleapis/googleapis` | `886464f687986b8b38392d83957a5a2ead4dcf71` (master, 2026-10-15) |
| `protocolbuffers/protobuf` (WKTs only) | tag `v35.1` (matches `protobuf/protoc` 35.1) |

## Files (sha256 at vendoring)

```
4c5b4cd802dc1e2a9e1500c01821924c6135b2a11388e79905c540d010c9082b  build/bazel/remote/execution/v2/remote_execution.proto
22b2af125690142af1c8152ba3a4ca15ffaa1265111dedc6e39b5412989b5be1  build/bazel/semver/semver.proto
e79ea741cb605a65e78ca322174764a4af9fde1962c1631e12b84c4934ba9a6c  google/api/annotations.proto
5e9fc117290fe7d3472791543c979bec7cf4497f3fe6ffaf9de9ae99cc4f1804  google/api/client.proto
e2a1ac88c49909d4579bacbb18519085e82da32c2c7bb15c31037f3be19f667f  google/api/field_behavior.proto
ead99129aa15dd5f6233942030c72eec33bc0d7b1c7c260dc143293ef66c5b78  google/api/http.proto
68e22c9821227b4e84c7029569fce1055c21e6ccab991e0986bc75f12ef4dd82  google/api/launch_stage.proto
961b833f35f4bdc51df4bca017cffdba299893e89762bf8041465560106dd3d6  google/bytestream/bytestream.proto
5d2e7f9eeb55b8417b50e0a536a092babd32d26e4fbeb7099c6cdcda6e16db9c  google/longrunning/operations.proto
8e56f61e3078e9232d39ce1b1bab28613783af8191f93a910c3617933f87a179  google/protobuf/any.proto
f46ec56fc26c48433f4cebfdc98cf1cdd5ae15f129f150256dfa82c1ec77e0a3  google/protobuf/descriptor.proto
a3f7301ff2956ec2e30c2241ece07197e4a86c752348d5607224819d4921c9fe  google/protobuf/duration.proto
ecef3d54cc9e079673b9816c67bac770f7f3bf6dada2d4596ba69d71daa971e6  google/protobuf/empty.proto
fa6c44dca2c90a2aaf3a1e4776cef2e9177f70fba365f236269f0f41ac166c96  google/protobuf/timestamp.proto
6878dc7534cf9805eee56345cdc38b5e9ca39dece246710c663c7007a67fef49  google/protobuf/wrappers.proto
f5bfd262e6705c7ae73f32e0ad8ee20ce8c0a2578df8c4f76ebf76b572f295ed  google/rpc/status.proto
```

## Refreshing

Re-download each path from `raw.githubusercontent.com` at the new pin
(`src/google/protobuf/...` for the `protobuf` repo), verify the import
closure is still self-contained:

```bash
cd remote_execution/reapid/protos
for i in $(grep -h "^import " $(find . -name '*.proto') | sed 's/import "\(.*\)";/\1/' | sort -u); do
  [ -f "$i" ] || echo "MISSING: $i"
done
```

then update the pins and hashes above. Never hand-edit a vendored file;
protoc parse failures after a refresh mean the closure grew — fetch the
new imports the same way.
