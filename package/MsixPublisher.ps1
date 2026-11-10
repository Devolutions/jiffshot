function Get-MsixPublisher {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [object]$Signature,
        [Parameter(Mandatory)]
        [bool]$RequireSigned
    )

    if ($Signature.Status -eq 'Valid') {
        if ($null -eq $Signature.SignerCertificate -or
            [string]::IsNullOrWhiteSpace($Signature.SignerCertificate.Subject)) {
            throw 'The executable signature is valid but has no signer subject.'
        }
        return $Signature.SignerCertificate.Subject
    }

    if ($RequireSigned) {
        throw "Cannot determine the MSIX publisher from an invalid executable signature: $($Signature.StatusMessage)"
    }
    return 'CN=Devolutions Jiffshot CI'
}
