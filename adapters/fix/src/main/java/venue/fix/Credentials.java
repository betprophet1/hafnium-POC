package venue.fix;

import java.util.regex.Pattern;

/**
 * The credential a FIX Logon carries, as the gateway's {@code login} takes it (interfaces/fix/README.md, sessions).
 *
 * <p>A firm sends either the account's dev token or an API key's whole credential ({@code keyId.secret}) in Password
 * (554), or the key ID in Username (553) and the secret in Password. Username is otherwise the firm's own and ignored,
 * so it is read as a key ID only when it is one.
 */
final class Credentials
{
    /** An API key's ID: {@code ak_} and 16 lower-case hex digits (interfaces/gateway/README.md). */
    private static final Pattern KEY_ID = Pattern.compile("ak_[0-9a-f]{16}");

    private Credentials()
    {
    }

    /** The credential for the gateway, from Username (null when absent) and Password; empty when there is none. */
    static String of(final String username, final String password)
    {
        if (password == null || password.isEmpty())
        {
            return "";
        }
        if (username != null && KEY_ID.matcher(username).matches() && !password.contains("."))
        {
            return username + "." + password;
        }
        return password;
    }
}
