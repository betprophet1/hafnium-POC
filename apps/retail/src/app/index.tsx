// Every market open for trading. The first screen of the app, and the one a person lands on from a shared link.
//
// Responsive by measurement, not by platform: the list is one column on a phone and as many as the width allows on a
// tablet or in a browser window. Nothing here asks what device it is running on.
//
// **The list is asked for again as it goes.** Rounds settle and new ones list while somebody is looking at the
// screen, so a list fetched once at mount is a picture of the venue as it was when the app opened: within minutes it
// names rounds that have finished and misses markets that have since opened. That is not a difference of opinion
// between the app and the venue — it is the app not having asked lately.

import { Link, router } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { ActivityIndicator, Pressable, RefreshControl, ScrollView, StyleSheet, View, useWindowDimensions } from 'react-native';

import { useVenue } from '@/venue/session';
import { Text } from '@/venue/text';
import type { Market } from '@/venue/types';

// How often the list is asked for again while somebody is on this screen.
const REFRESH_MS = 15_000;

export default function Markets() {
  const { venue, member, live } = useVenue();
  const [markets, setMarkets] = useState<Market[] | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const { width } = useWindowDimensions();
  // One column on a phone; more as there is room for them, at about 320 points each.
  const columns = Math.max(1, Math.min(4, Math.floor(width / 320)));

  const load = useCallback(async () => {
    try {
      setMarkets(await venue.markets());
      setFailed(null);
    } catch (e) {
      setFailed(e instanceof Error ? e.message : String(e));
    }
  }, [venue]);

  useEffect(() => {
    void load();
    // Often enough that a round changing is noticed, rarely enough that it costs nothing: a round is minutes, and
    // this is one small request. The venue offers the same thing as a stream (`markets` on the public WebSocket),
    // which is where this should end up — pushed rather than asked for — but that wants the connection handling the
    // private stream already has, and a list that is a few seconds old is not what was wrong here.
    const again = setInterval(() => void load(), REFRESH_MS);
    return () => clearInterval(again);
  }, [load]);

  return (
    <ScrollView
      contentContainerStyle={styles.page}
      refreshControl={
        <RefreshControl
          refreshing={refreshing}
          onRefresh={() => {
            setRefreshing(true);
            void load().finally(() => setRefreshing(false));
          }}
        />
      }>
      <View style={styles.bar}>
        <Text style={styles.status}>{member ? `${member.email}${live ? ' · live' : ' · connecting'}` : 'Not signed in'}</Text>
        <Link href="/parlays" style={styles.link}>
          Parlays
        </Link>
        <Link href="/account" style={styles.link}>
          {member ? 'Account' : 'Sign in'}
        </Link>
      </View>

      {failed ? <Text style={styles.failed}>{failed}</Text> : null}
      {markets === null && !failed ? <ActivityIndicator /> : null}

      <View style={[styles.grid, { flexDirection: columns > 1 ? 'row' : 'column' }]}>
        {(markets ?? []).map((market) => (
          <Pressable
            key={market.instrumentId}
            onPress={() => router.push(`/market/${market.instrumentId}`)}
            style={[styles.card, { width: columns > 1 ? `${100 / columns - 2}%` : '100%' }]}>
            <Text style={styles.title}>{market.title}</Text>
            <Text style={styles.label}>{market.label}</Text>
            <Text style={styles.state}>{market.state.toLowerCase()}</Text>
          </Pressable>
        ))}
      </View>

      {markets?.length === 0 ? <Text style={styles.empty}>No markets are open.</Text> : null}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 12, maxWidth: 1200, width: '100%', alignSelf: 'center' },
  bar: { flexDirection: 'row', justifyContent: 'space-between', alignItems: 'center' },
  status: { opacity: 0.7 },
  link: { fontWeight: '600' },
  grid: { flexWrap: 'wrap', gap: 12 },
  card: { padding: 14, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888', gap: 4 },
  title: { fontSize: 16, fontWeight: '600' },
  label: { opacity: 0.8 },
  state: { opacity: 0.5, fontSize: 12 },
  failed: { color: '#b00020' },
  empty: { opacity: 0.6 },
});
