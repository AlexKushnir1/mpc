near call v1.signer-dev.testnet sign '{"request":{"key_version":0,"path":"test","payload":[12,1,2,0,4,5,6,8,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,44]}}' --accountId alexkushnir.testnet --gas 300000000000000 --deposit 1

near call v1.signer-dev.testnet public_key --accountId alexkushnir.testnet --gas 300000000000000 --deposit 1

near call v1.signer-dev.testnet derived_public_key '{"path":"test","predecessor":"alexkushnir.testnet"}' --accountId alexkushnir.testnet --gas 300000000000000 --deposit 1

