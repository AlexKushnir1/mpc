# MPC Account Recovery (WIP)
The aim of this project is to offer NEAR users the opportunity to restore their accounts by utilizing OAuth authorization. By linking their NEAR account to Gmail, Github, or other authentication provider, they can then add a new Full Access key, which will be managed by the trusted network of servers. Should they lose all the keys they possess, they can reauthorize themselves, create a new key, and add it into their NEAR account using a transaction that will be signed by MPC servers through their recovery key.

## Adding a recovery method
1. The user is getting OAuth access token (AT) from their authentication provider
2. The user is signing this AT with their NEAR private key
3. The user is sending the created payload to the multi-party computation system (MPC, or just "server").
4. Server checks the AT
5. Server fetches the list of user keys and checks the signature
6. If all the checks were successful, server adds recovery method to it's database and generates a new key using Key Derivation technique
7. User gets the public key (PK) from the server and adds it as a Full Access key to their NEAR account.


## Using previously added recovery method
1. The user is getting OAuth access token from it's authentication provider
2. The user generates a new key they want to add to their NEAR account
3. The user sends the AT alongside with PK to the server
4. Server checks the AT
5. Server adds the provided PK to the users account

## How the MPC system works
- The system consists of N (4+) trusted nodes
- Each node holds a unique secret key
- Each action must be signed by N-1 node

## External API
Endpoint 1: Add Recovery Method

    URL: /add_recovery_method
    Request parameters: access_token, signature, accountId
    Response: recovery_public_key

Endpoint 2: Recover Account

    URL: /recover_account
    Request parameters: access_token, public_key
    Response: status