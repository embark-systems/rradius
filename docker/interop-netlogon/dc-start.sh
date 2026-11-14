#!/bin/sh
# Provisions the domain CORP (corp.example) with the accounts run.sh checks, then runs the DC.
# Netlogon listens on a fixed port, 49200, so that it can be published. After a restart the
# domain is kept as it was. /var/lib/samba/provisioned marks that every account is set up.
set -eu
NTLM_AUTH=${NTLM_AUTH:-mschapv2-and-ntlmv2-only}
if [ ! -f /var/lib/samba/provisioned ]; then
    rm -f /etc/samba/smb.conf
    samba-tool domain provision --realm=CORP.EXAMPLE --domain=CORP --server-role=dc \
        --dns-backend=SAMBA_INTERNAL --adminpass='Admin-Pass-1' \
        --option="rpc server port:netlogon = 49200" --option="ntlm auth = $NTLM_AUTH" >/dev/null
    samba-tool user create alice 'Alice-Pass-1' >/dev/null
    samba-tool user create bob 'Bob-Pass-1' >/dev/null
    samba-tool user create carol 'Carol-Pass-1' >/dev/null
    samba-tool user disable carol >/dev/null
    samba-tool group add wifi-staff >/dev/null
    samba-tool group addmembers wifi-staff alice >/dev/null
    samba-tool computer create RRADIUS1 >/dev/null
    samba-tool user setpassword 'RRADIUS1$' --newpassword='Machine-Pass-1' >/dev/null
    touch /var/lib/samba/provisioned
    echo "provisioned"
fi
exec samba --foreground --no-process-group --debug-stdout
