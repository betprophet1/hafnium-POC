// Signing in, and what the venue says about this account once you are.
//
// The same screen serves both: nobody is signed in, so it asks; somebody is, so it shows. What it shows is pushed by
// the venue rather than polled (`session.tsx`), so a fill on another device appears here without this one asking.
//
// Signing in is a few steps rather than one, and which of them a person sees depends on what the venue answers, not
// on what this screen guesses:
//
// - **Signing up** answers only "check your mail" — never whether the address is already somebody's — so the screen
//   asks for the code from that message instead of pretending a session came back.
// - **A second factor** shows up as `SECOND_FACTOR_REQUIRED` on a login whose password was right, which is the one
//   refusal that asks for something different rather than the same thing again.
// - **Unlocking** is offered when the phone is holding a session behind its lock. It is this device letting go of a
//   credential it already has; the venue is not told and does not care (`storage.ts`).

import { Link } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { ActivityIndicator, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';

import { VenueError } from '@/venue/types';
import type { HeldSession } from '@/venue/types';
import { useVenue } from '@/venue/session';

const money = (cents: number) => `$${(cents / 100).toFixed(2)}`;

/** Which of the signed-out screens is showing. `code` follows a sign-up: the address is waiting to be proved. */
type Step = 'signIn' | 'code' | 'reset';

export default function AccountScreen() {
  const venue = useVenue();
  const { member, account, about, starting, live, locked, unlock, unlockWith } = venue;
  const [step, setStep] = useState<Step>('signIn');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [wantsCode, setWantsCode] = useState(false);
  const [token, setToken] = useState('');
  const [said, setSaid] = useState<string | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  /** Runs one step, and puts whatever the venue said where a person can read it. */
  const attempt = useCallback(async (what: () => Promise<string | null>) => {
    setBusy(true);
    setFailed(null);
    setSaid(null);
    try {
      setSaid(await what());
    } catch (e) {
      if (e instanceof VenueError && e.code === 'SECOND_FACTOR_REQUIRED') {
        // The password was right: ask for the code rather than for the password again.
        setWantsCode(true);
        setFailed('This account asks for a code from your authenticator app.');
      } else {
        setFailed(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setBusy(false);
    }
  }, []);

  if (starting) {
    return (
      <View style={styles.centre}>
        <ActivityIndicator />
      </View>
    );
  }

  if (!member) {
    return (
      <ScrollView contentContainerStyle={styles.page}>
        {locked ? (
          <View style={styles.card}>
            <Text style={styles.heading}>Welcome back</Text>
            <Text style={styles.note}>
              This phone is holding your session. Unlock it with {unlockWith}, or sign in with your password.
            </Text>
            <Pressable
              disabled={busy}
              onPress={() =>
                void attempt(async () => {
                  const opened = await venue.unlockSession();
                  return opened ? null : 'That did not unlock it. Sign in with your password instead.';
                })
              }
              style={[styles.action, busy && styles.disabled]}
            >
              <Text>Unlock with {unlockWith}</Text>
            </Pressable>
          </View>
        ) : null}

        <Text style={styles.heading}>{step === 'reset' ? 'Forgotten password' : 'Sign in'}</Text>
        <TextInput
          value={email}
          onChangeText={setEmail}
          placeholder="you@example.com"
          autoCapitalize="none"
          autoComplete="username"
          keyboardType="email-address"
          style={styles.field}
          accessibilityLabel="Email"
        />

        {step === 'signIn' ? (
          <>
            <TextInput
              value={password}
              onChangeText={setPassword}
              placeholder="at least 10 characters"
              secureTextEntry
              autoComplete="current-password"
              style={styles.field}
              accessibilityLabel="Password"
            />
            {wantsCode ? (
              <TextInput
                value={code}
                onChangeText={setCode}
                placeholder="123456, or a recovery code"
                autoCapitalize="none"
                autoComplete="one-time-code"
                keyboardType="numeric"
                style={styles.field}
                accessibilityLabel="Code from your authenticator"
              />
            ) : null}
            <View style={styles.row}>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    await venue.logIn(email.trim(), password, code.trim() || undefined);
                    setPassword('');
                    setCode('');
                    setWantsCode(false);
                    return null;
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Log in</Text>
              </Pressable>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    await venue.signUp(email.trim(), password);
                    setPassword('');
                    setStep('code');
                    return 'Check your mail: we have sent a code to prove that address.';
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Sign up</Text>
              </Pressable>
            </View>
            <Pressable onPress={() => setStep('reset')} style={styles.quiet}>
              <Text style={styles.note}>I have forgotten my password</Text>
            </Pressable>
            <Pressable onPress={() => setStep('code')} style={styles.quiet}>
              <Text style={styles.note}>I have a code to prove my address</Text>
            </Pressable>
          </>
        ) : null}

        {step === 'code' ? (
          <>
            <TextInput
              value={token}
              onChangeText={setToken}
              placeholder="the code from the message"
              autoCapitalize="none"
              style={styles.field}
              accessibilityLabel="Code from the message"
            />
            <View style={styles.row}>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    await venue.venue.verifyEmail(token.trim());
                    setToken('');
                    setStep('signIn');
                    return 'That address is proved. Log in and you can move money as well as trade.';
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Prove it</Text>
              </Pressable>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    await venue.venue.resendVerification(email.trim());
                    return 'Check your mail: if that address has an account waiting, a new code is on its way.';
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Send it again</Text>
              </Pressable>
            </View>
            <Pressable onPress={() => setStep('signIn')} style={styles.quiet}>
              <Text style={styles.note}>Back to signing in</Text>
            </Pressable>
          </>
        ) : null}

        {step === 'reset' ? (
          <>
            <TextInput
              value={token}
              onChangeText={setToken}
              placeholder="the code from the message, once you have it"
              autoCapitalize="none"
              style={styles.field}
              accessibilityLabel="Code from the message"
            />
            <TextInput
              value={password}
              onChangeText={setPassword}
              placeholder="your new password, at least 10 characters"
              secureTextEntry
              autoComplete="new-password"
              style={styles.field}
              accessibilityLabel="New password"
            />
            <View style={styles.row}>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    await venue.venue.requestReset(email.trim());
                    return 'Check your mail: if that address has an account, a code is on its way.';
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Send a code</Text>
              </Pressable>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void attempt(async () => {
                    const set = await venue.venue.setPassword(token.trim(), password);
                    setToken('');
                    setPassword('');
                    setStep('signIn');
                    const n = set.sessionsEnded;
                    return `Password set. ${n} session${n === 1 ? '' : 's'} elsewhere ${n === 1 ? 'was' : 'were'} signed out.`;
                  })
                }
                style={[styles.action, busy && styles.disabled]}
              >
                <Text>Set it</Text>
              </Pressable>
            </View>
            <Pressable onPress={() => setStep('signIn')} style={styles.quiet}>
              <Text style={styles.note}>Back to signing in</Text>
            </Pressable>
          </>
        ) : null}

        {said ? <Text style={styles.said}>{said}</Text> : null}
        {failed ? <Text style={styles.failed}>{failed}</Text> : null}
        <Where />
      </ScrollView>
    );
  }

  const proved = about ? about.emailVerified : member.emailVerified;

  return (
    <ScrollView contentContainerStyle={styles.page}>
      <Text style={styles.heading}>{member.displayName}</Text>
      <Text style={styles.note}>
        {member.email} · account {member.accountId} · {live ? 'live' : 'connecting'}
      </Text>

      {proved ? null : (
        <View style={styles.card}>
          <Text>Your address is not proved yet.</Text>
          <Text style={styles.note}>
            You can trade what you have. Moving money in or out waits until you use the code we sent you.
          </Text>
          <TextInput
            value={token}
            onChangeText={setToken}
            placeholder="the code from the message"
            autoCapitalize="none"
            style={styles.field}
            accessibilityLabel="Code from the message"
          />
          <View style={styles.row}>
            <Pressable
              disabled={busy}
              onPress={() =>
                void attempt(async () => {
                  await venue.venue.verifyEmail(token.trim());
                  setToken('');
                  await venue.refreshAbout();
                  return 'That address is proved.';
                })
              }
              style={[styles.action, busy && styles.disabled]}
            >
              <Text>Prove it</Text>
            </Pressable>
            <Pressable
              disabled={busy}
              onPress={() =>
                void attempt(async () => {
                  await venue.venue.resendVerification(member.email);
                  return 'A new code is on its way.';
                })
              }
              style={[styles.action, busy && styles.disabled]}
            >
              <Text>Send it again</Text>
            </Pressable>
          </View>
        </View>
      )}

      <View style={styles.cards}>
        <View style={styles.card}>
          <Text style={styles.note}>Available</Text>
          <Text style={styles.figure}>{account ? money(account.availableCents) : '—'}</Text>
        </View>
        <View style={styles.card}>
          <Text style={styles.note}>Reserved</Text>
          <Text style={styles.figure}>{account ? money(account.reservedCents) : '—'}</Text>
        </View>
      </View>

      <Text style={styles.heading}>Positions</Text>
      {account?.positions.length ? (
        account.positions.map((position) => (
          <View key={position.instrumentId} style={styles.line}>
            <Link href={`/market/${position.instrumentId}`}>{position.instrumentId}</Link>
            <Text>{position.quantity}</Text>
          </View>
        ))
      ) : (
        <Text style={styles.note}>none</Text>
      )}

      <Text style={styles.heading}>Resting orders</Text>
      {account?.orders.length ? (
        account.orders.map((order) => (
          <View key={order.orderId} style={styles.line}>
            <Link href={`/market/${order.instrumentId}`}>
              {order.side} {order.remaining} at {order.price.cents}¢
            </Link>
            <Text style={styles.note}>{order.orderId}</Text>
          </View>
        ))
      ) : (
        <Text style={styles.note}>none</Text>
      )}

      <Security busy={busy} attempt={attempt} unlock={unlock} unlockWith={unlockWith} />

      {said ? <Text style={styles.said}>{said}</Text> : null}
      {failed ? <Text style={styles.failed}>{failed}</Text> : null}

      <Where />

      <Pressable onPress={() => void venue.logOut()} style={styles.action}>
        <Text>Log out</Text>
      </Pressable>
      <Text style={styles.note}>
        A new account has no money. Deposit on the venue&apos;s own page to trade in the demo.
      </Text>
    </ScrollView>
  );
}

/** The second factor, and the sessions this member holds elsewhere. Its own component because it reads the venue
 * when it appears and after anything it changes, which the rest of the screen does not. */
function Security({
  busy,
  attempt,
  unlock,
  unlockWith,
}: {
  busy: boolean;
  attempt: (what: () => Promise<string | null>) => Promise<void>;
  unlock: string;
  unlockWith: string;
}) {
  const venue = useVenue();
  const { about, member } = venue;
  const [held, setHeld] = useState<HeldSession[]>([]);
  const [starting, setStarting] = useState<{ secret: string; uri: string } | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [code, setCode] = useState('');
  const [password, setPassword] = useState('');

  const reload = useCallback(async () => {
    try {
      setHeld(await venue.sessions());
    } catch {
      // The screen keeps the last list rather than emptying it over one failed read.
    }
    await venue.refreshAbout();
  }, [venue]);

  useEffect(() => {
    void reload();
    // When this screen appears, and whenever the member changes.
  }, [reload]);

  return (
    <>
      <Text style={styles.heading}>Security</Text>

      {unlock === 'none' ? null : (
        <Text style={styles.note}>
          This device is holding your session behind {unlockWith}. That unlocks what is on the phone; it is not how
          the venue knows you.
        </Text>
      )}

      {codes ? (
        <View style={styles.card}>
          <Text>Your recovery codes</Text>
          <Text style={styles.note}>
            Ten, each good once, for when the authenticator is gone. They are kept only as hashes, so this is the
            only time anyone sees them — write them down now.
          </Text>
          {codes.map((c) => (
            <Text key={c} style={styles.code}>
              {c}
            </Text>
          ))}
          <Pressable onPress={() => setCodes(null)} style={styles.action}>
            <Text>Done</Text>
          </Pressable>
        </View>
      ) : null}

      {starting ? (
        <View style={styles.card}>
          <Text>Put this in an authenticator app</Text>
          <Text style={styles.note}>
            By camera or by hand, then type the code it shows. Nothing changes until that code arrives, so a secret
            typed wrong cannot lock you out.
          </Text>
          <Text style={styles.code}>{starting.secret}</Text>
          <TextInput
            value={code}
            onChangeText={setCode}
            placeholder="123456"
            keyboardType="numeric"
            style={styles.field}
            accessibilityLabel="Code from your authenticator"
          />
          <Pressable
            disabled={busy}
            onPress={() =>
              void attempt(async () => {
                const done = await venue.venue.confirmSecondFactor(code.trim());
                setCode('');
                setStarting(null);
                setCodes(done.recoveryCodes);
                await reload();
                return null;
              })
            }
            style={[styles.action, busy && styles.disabled]}
          >
            <Text>Confirm</Text>
          </Pressable>
        </View>
      ) : null}

      {about?.secondFactor ? (
        <View style={styles.card}>
          <Text>
            A second factor is on, with {about.recoveryCodesLeft} recovery{' '}
            {about.recoveryCodesLeft === 1 ? 'code' : 'codes'} left.
          </Text>
          <TextInput
            value={password}
            onChangeText={setPassword}
            placeholder="your password, to take it off"
            secureTextEntry
            autoComplete="current-password"
            style={styles.field}
            accessibilityLabel="Password"
          />
          <Pressable
            disabled={busy}
            onPress={() =>
              void attempt(async () => {
                await venue.venue.removeSecondFactor(password);
                setPassword('');
                await reload();
                return 'The second factor is off.';
              })
            }
            style={[styles.action, busy && styles.disabled]}
          >
            <Text>Take it off</Text>
          </Pressable>
        </View>
      ) : starting ? null : (
        <Pressable
          disabled={busy}
          onPress={() =>
            void attempt(async () => {
              setStarting(await venue.venue.startSecondFactor());
              return null;
            })
          }
          style={[styles.action, busy && styles.disabled]}
        >
          <Text>Add a second factor</Text>
        </Pressable>
      )}

      <Text style={styles.heading}>Where you are signed in</Text>
      {held.filter((s) => s.live).length ? (
        held
          .filter((s) => s.live)
          .map((s) => {
            const here = s.sessionId === member?.session.sessionId;
            return (
              <View key={s.sessionId} style={styles.line}>
                <Text>
                  {s.device ?? 'an app'}
                  {here ? ' (this one)' : ''}
                </Text>
                {here ? (
                  <Text style={styles.note}>{s.lastUsedAt ?? s.createdAt}</Text>
                ) : (
                  <Pressable
                    disabled={busy}
                    onPress={() =>
                      void attempt(async () => {
                        await venue.venue.endSession(s.sessionId);
                        await reload();
                        return 'That session is signed out.';
                      })
                    }
                    style={[styles.quiet, busy && styles.disabled]}
                  >
                    <Text style={styles.note}>End it</Text>
                  </Pressable>
                )}
              </View>
            );
          })
      ) : (
        <Text style={styles.note}>none</Text>
      )}
    </>
  );
}

/** Which venue the app is talking to, and — for somebody the venue offers a choice — how to move between them.
 *
 * A venue that is not the live one is said so plainly, because the whole hazard of test environments is somebody
 * trading imaginary money while believing it is real. Moving signs you out: a session belongs to the venue that
 * issued it and is nobody else's, so there is nothing to carry across (interface 1 README, choosing an environment).
 */
function Where() {
  const { environment, environments, chooseEnvironment } = useVenue();
  const elsewhere = environments.filter((e) => e.url !== environment.url);
  return (
    <View style={styles.card}>
      <Text style={styles.note}>
        Talking to {environment.label} · {environment.url}
      </Text>
      {environment.live ? null : (
        <Text style={styles.failed}>This is not the live venue. Nothing here is real money.</Text>
      )}
      {elsewhere.length ? (
        <>
          <Text style={styles.note}>Switching signs you out: a session belongs to the venue that issued it.</Text>
          {elsewhere.map((e) => (
            <Pressable key={e.id} onPress={() => void chooseEnvironment(e)} style={styles.action}>
              <Text>
                Switch to {e.label}
                {e.live ? ' (live)' : ''}
              </Text>
            </Pressable>
          ))}
        </>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 12, maxWidth: 700, width: '100%', alignSelf: 'center' },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  heading: { fontSize: 18, fontWeight: '600', marginTop: 8 },
  field: { padding: 12, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  row: { flexDirection: 'row', gap: 10 },
  cards: { flexDirection: 'row', gap: 12 },
  card: { flex: 1, padding: 14, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888', gap: 10 },
  figure: { fontSize: 22, fontWeight: '600' },
  code: { fontFamily: 'monospace', fontSize: 16 },
  line: { flexDirection: 'row', justifyContent: 'space-between', paddingVertical: 6 },
  action: { alignItems: 'center', padding: 12, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888', flex: 1 },
  quiet: { paddingVertical: 6 },
  disabled: { opacity: 0.4 },
  note: { opacity: 0.6 },
  said: { color: '#06603a' },
  failed: { color: '#b00020' },
});
