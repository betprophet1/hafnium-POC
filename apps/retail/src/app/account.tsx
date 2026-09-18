// Signing in, and what the venue says about this account once you are.
//
// The same screen serves both: nobody is signed in, so it asks; somebody is, so it shows. What it shows is pushed by
// the venue rather than polled (`session.tsx`), so a fill on another device appears here without this one asking.

import { Link } from 'expo-router';
import { useState } from 'react';
import { ActivityIndicator, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';

import { useVenue, VENUE_URL } from '@/venue/session';

const money = (cents: number) => `$${(cents / 100).toFixed(2)}`;

export default function AccountScreen() {
  const { member, account, starting, live, signUp, logIn, logOut } = useVenue();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [failed, setFailed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const attempt = async (what: (email: string, password: string) => Promise<void>) => {
    setBusy(true);
    setFailed(null);
    try {
      await what(email.trim(), password);
      setPassword('');
    } catch (e) {
      setFailed(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

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
        <Text style={styles.heading}>Sign in</Text>
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
        <TextInput
          value={password}
          onChangeText={setPassword}
          placeholder="at least 10 characters"
          secureTextEntry
          autoComplete="current-password"
          style={styles.field}
          accessibilityLabel="Password"
        />
        <View style={styles.row}>
          <Pressable disabled={busy} onPress={() => void attempt(logIn)} style={[styles.action, busy && styles.disabled]}>
            <Text>Log in</Text>
          </Pressable>
          <Pressable disabled={busy} onPress={() => void attempt(signUp)} style={[styles.action, busy && styles.disabled]}>
            <Text>Sign up</Text>
          </Pressable>
        </View>
        {failed ? <Text style={styles.failed}>{failed}</Text> : null}
        <Text style={styles.note}>Talking to {VENUE_URL}</Text>
      </ScrollView>
    );
  }

  return (
    <ScrollView contentContainerStyle={styles.page}>
      <Text style={styles.heading}>{member.displayName}</Text>
      <Text style={styles.note}>
        {member.email} · account {member.accountId} · {live ? 'live' : 'connecting'}
      </Text>

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

      <Pressable onPress={() => void logOut()} style={styles.action}>
        <Text>Log out</Text>
      </Pressable>
      <Text style={styles.note}>
        A new account has no money. Deposit on the venue&apos;s own page to trade in the demo.
      </Text>
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 12, maxWidth: 700, width: '100%', alignSelf: 'center' },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  heading: { fontSize: 18, fontWeight: '600', marginTop: 8 },
  field: { padding: 12, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  row: { flexDirection: 'row', gap: 10 },
  cards: { flexDirection: 'row', gap: 12 },
  card: { flex: 1, padding: 14, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  figure: { fontSize: 22, fontWeight: '600' },
  line: { flexDirection: 'row', justifyContent: 'space-between', paddingVertical: 6 },
  action: { alignItems: 'center', padding: 12, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888', flex: 1 },
  disabled: { opacity: 0.4 },
  note: { opacity: 0.6 },
  failed: { color: '#b00020' },
});
