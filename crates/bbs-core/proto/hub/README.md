# Vendored Hub protocol

Copied from github.com/neuralwatt/hub branch `rooms-v2` at commit fe8baa5
(`proto/bbs/integration/v1/integration.proto` and the
`proto/msgproto/v1/messaging.proto` it imports, and the federation stream
`proto/msgproto/v1/federation.proto` with the `security.proto` it imports).
Do not edit the msgproto files; replace them with a newer Hub commit's copies
and update this line. `integration.proto` is BBS's: this copy adds the room
request frames (`RoomResult`, `remote_room_request`, `remote_room_result`),
which go to Hub's copy as the same change.
