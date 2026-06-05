Set up an IRC adapter for testing.

Create a built-in IRC adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `libera-exo-test`
- server: `irc.libera.chat`
- port: `6697`
- tls: `true`
- nick: `exospooky`
- username: `exospooky`
- realname: `Exoclaw Test Bot`
- channel: `#exoclaw`
- passwordSecretId: `null`
- trigger: `mention`

After creating or confirming the adapter, briefly tell me the adapter id, nick, channel, and exactly what message I should send from IRC to test it.
