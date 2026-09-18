// One market: what it is trading at, and a ticket to buy or sell it.
//
// The price a ticket sends is a tick from the book, never a number the app made up: the ladder belongs to the venue
// (interfaces/gateway/README.md, prices), and an app that invents prices gets them refused. Tapping a rung is how a
// person picks one.

import { useLocalSearchParams } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { ActivityIndicator, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';

import { useVenue } from '@/venue/session';
import type { Book, Level, OrderAnswer, Outcome } from '@/venue/types';

export default function MarketScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const { venue, member, refresh } = useVenue();
  const [book, setBook] = useState<Book | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [tick, setTick] = useState<number | null>(null);
  const [quantity, setQuantity] = useState('1');
  const [outcome, setOutcome] = useState<Outcome>('yes');
  const [answer, setAnswer] = useState<OrderAnswer | null>(null);
  const [sending, setSending] = useState(false);

  const load = useCallback(async () => {
    if (!id) return;
    try {
      setBook(await venue.book(id));
      setFailed(null);
    } catch (e) {
      setFailed(e instanceof Error ? e.message : String(e));
    }
  }, [venue, id]);

  useEffect(() => {
    void load();
    // The public book is conflated by the venue at ten a second; this screen asks for it again every two seconds,
    // which is enough to watch a market and far less than streaming every change to a phone.
    const again = setInterval(() => void load(), 2000);
    return () => clearInterval(again);
  }, [load]);

  const send = async (action: 'buy' | 'sell') => {
    if (!id || tick === null) return;
    setSending(true);
    setAnswer(null);
    try {
      const placed = await venue.placeOrder({
        clientOrderId: `app-${Date.now().toString(36)}`,
        instrumentId: id,
        action,
        outcome,
        price: { tick },
        quantity: Math.max(1, Number(quantity) || 1),
      });
      setAnswer(placed);
      await refresh();
      await load();
    } catch (e) {
      setFailed(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
    }
  };

  return (
    <ScrollView contentContainerStyle={styles.page}>
      {failed ? <Text style={styles.failed}>{failed}</Text> : null}
      {book === null && !failed ? <ActivityIndicator /> : null}

      <View style={styles.books}>
        <Side title="Bids" levels={book?.bids ?? []} picked={tick} onPick={setTick} />
        <Side title="Asks" levels={book?.asks ?? []} picked={tick} onPick={setTick} />
      </View>

      {!member ? (
        <Text style={styles.note}>Sign in on the account screen to trade.</Text>
      ) : (
        <View style={styles.ticket}>
          <Text style={styles.heading}>{tick === null ? 'Pick a price from the book' : `At tick ${tick}`}</Text>
          <View style={styles.row}>
            {(['yes', 'no'] as const).map((side) => (
              <Pressable
                key={side}
                onPress={() => setOutcome(side)}
                style={[styles.choice, outcome === side && styles.chosen]}>
                <Text>{side}</Text>
              </Pressable>
            ))}
            <TextInput
              value={quantity}
              onChangeText={setQuantity}
              keyboardType="number-pad"
              style={styles.quantity}
              accessibilityLabel="Contracts"
            />
          </View>
          <View style={styles.row}>
            <Pressable
              disabled={tick === null || sending}
              onPress={() => void send('buy')}
              style={[styles.action, (tick === null || sending) && styles.disabled]}>
              <Text>Buy</Text>
            </Pressable>
            <Pressable
              disabled={tick === null || sending}
              onPress={() => void send('sell')}
              style={[styles.action, (tick === null || sending) && styles.disabled]}>
              <Text>Sell</Text>
            </Pressable>
          </View>
          {answer ? (
            <Text style={answer.status === 'rejected' ? styles.failed : styles.ok}>
              {answer.status === 'rejected'
                ? `Rejected: ${(answer.rejectReason ?? '').replaceAll('_', ' ')}`
                : `${answer.status}${answer.orderId ? ` · order ${answer.orderId}` : ''}`}
            </Text>
          ) : null}
        </View>
      )}
    </ScrollView>
  );
}

function Side({
  title,
  levels,
  picked,
  onPick,
}: {
  title: string;
  levels: Level[];
  picked: number | null;
  onPick: (tick: number) => void;
}) {
  return (
    <View style={styles.side}>
      <Text style={styles.heading}>{title}</Text>
      {levels.length === 0 ? <Text style={styles.note}>nothing resting</Text> : null}
      {levels.map((level) => (
        <Pressable
          key={level.price.tick}
          onPress={() => onPick(level.price.tick)}
          style={[styles.level, picked === level.price.tick && styles.chosen]}>
          <Text>{level.price.cents}¢</Text>
          <Text style={styles.note}>{level.quantity}</Text>
        </Pressable>
      ))}
    </View>
  );
}

const styles = StyleSheet.create({
  page: { padding: 16, gap: 16, maxWidth: 900, width: '100%', alignSelf: 'center' },
  books: { flexDirection: 'row', gap: 16 },
  side: { flex: 1, gap: 6 },
  heading: { fontWeight: '600' },
  level: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    padding: 10,
    borderRadius: 8,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: '#8888',
  },
  ticket: { gap: 10, padding: 14, borderRadius: 10, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  row: { flexDirection: 'row', gap: 10, alignItems: 'center' },
  choice: { paddingVertical: 8, paddingHorizontal: 14, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  chosen: { borderColor: '#2f6feb', borderWidth: 2 },
  quantity: { flex: 1, padding: 8, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  action: { flex: 1, alignItems: 'center', padding: 12, borderRadius: 8, borderWidth: StyleSheet.hairlineWidth, borderColor: '#8888' },
  disabled: { opacity: 0.4 },
  note: { opacity: 0.6 },
  failed: { color: '#b00020' },
  ok: { color: '#1b7f3b' },
});
