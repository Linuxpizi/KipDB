use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::hash_map::{Iter, RandomState};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::error::CacheError;

pub type Result<T> = std::result::Result<T, CacheError>;

// 只读Node操作裸指针
// https://course.rs/advance/concurrency-with-threads/send-sync.html#:~:text=%E5%AE%89%E5%85%A8%E7%9A%84%E4%BD%BF%E7%94%A8%E3%80%82-,%E4%B8%BA%E8%A3%B8%E6%8C%87%E9%92%88%E5%AE%9E%E7%8E%B0Send,-%E4%B8%8A%E9%9D%A2%E6%88%91%E4%BB%AC%E6%8F%90%E5%88%B0
// 通过只读数据已保证线程安全
struct NodeReadPtr<K, V>(NonNull<Node<K, V>>);

unsafe impl<K: Send, V: Send> Send for NodeReadPtr<K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for NodeReadPtr<K, V> {}

impl<K, V> Clone for NodeReadPtr<K, V> {
    fn clone(&self) -> Self {
        NodeReadPtr(self.0)
    }
}

impl<K, V> Copy for NodeReadPtr<K, V> {

}

impl<K, V> Deref for NodeReadPtr<K, V> {
    type Target = NonNull<Node<K, V>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K, V> DerefMut for NodeReadPtr<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

unsafe impl<K: Send, V: Send, S: Send> Send for ShardingLruCache<K, V, S> {}
unsafe impl<K: Sync, V: Sync, S: Sync> Sync for ShardingLruCache<K, V, S> {}

pub(crate) struct ShardingLruCache<K, V, S = RandomState> {
    sharding_vec: Vec<Arc<Mutex<LruCache<K, V>>>>,
    hasher: S,
}

struct Node<K, V> {
    key: K,
    value: V,
    prev: Option<NodeReadPtr<K, V>>,
    next: Option<NodeReadPtr<K, V>>,
}

struct KeyRef<K, V>(NodeReadPtr<K, V>);

impl<K: Hash + Eq, V> Borrow<K> for KeyRef<K, V> {
    fn borrow(&self) -> &K {
        unsafe { &self.0.as_ref().key }
    }
}

impl<K: Hash, V> Hash for KeyRef<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe { self.0.as_ref().key.hash(state) }
    }
}

impl<K: Eq, V> Eq for KeyRef<K, V> {}

impl<K: Eq, V> PartialEq<Self> for KeyRef<K, V> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { self.0.as_ref().key.eq(&other.0.as_ref().key) }
    }
}

impl<K: Ord, V> PartialOrd<Self> for KeyRef<K, V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        unsafe { self.0.as_ref().key.partial_cmp(&other.0.as_ref().key) }
    }
}

impl<K: Ord, V> Ord for KeyRef<K, V>  {
    fn cmp(&self, other: &Self) -> Ordering {
        unsafe { self.0.as_ref().key.cmp(&other.0.as_ref().key) }
    }
}

/// LRU缓存
/// 参考知乎中此文章的实现：
/// https://zhuanlan.zhihu.com/p/466409120
pub(crate) struct LruCache<K, V> {
    head: Option<NodeReadPtr<K, V>>,
    tail: Option<NodeReadPtr<K, V>>,
    inner: HashMap<KeyRef<K, V>, NodeReadPtr<K, V>>,
    cap: usize,
    marker: PhantomData<Node<K, V>>,
}

impl<K, V> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        Self {
            key,
            value,
            prev: None,
            next: None,
        }
    }
}

impl<K: Hash + Eq + PartialEq, V, S: BuildHasher> ShardingLruCache<K, V, S> {
    pub(crate) fn new(cap: usize, sharding_size: usize, hasher: S) -> Result<Self> {
        let mut sharding_vec = Vec::with_capacity(sharding_size);
        if cap % sharding_size != 0 {
            return Err(CacheError::ShardingNotAlign);
        }
        let sharding_cap = cap / sharding_size;
        for _ in 0..sharding_size {
            sharding_vec.push(Arc::new(Mutex::new(LruCache::new(sharding_cap)?)));
        }

        Ok(ShardingLruCache {
            sharding_vec,
            hasher,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.shard(key)
            .lock()
            .get_node(key)
            .map(|node| {
                unsafe { &node.as_ref().value }
            })
    }

    pub(crate) fn put(&self, key: K, value: V) -> Option<V> {
        self.shard(&key)
            .lock()
            .put(key, value)
    }

    pub(crate) fn remove(&self, key: &K) -> Option<V> {
        self.shard(key)
            .lock()
            .remove(key)
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        for lru in &self.sharding_vec {
            if !lru.lock().is_empty() {
                return false
            }
        }
        true
    }

    pub(crate) fn get_or_insert<F>(
        &self,
        key: K,
        fn_once: F
    ) -> Result<&V>
        where F: FnOnce(&K) -> Result<V>
    {
        self.shard(&key)
            .lock()
            .get_or_insert_node(key, fn_once)
            .map(|node| unsafe { &node.as_ref().value })
    }

    fn sharding_size(&self) -> usize {
        self.sharding_vec.len()
    }

    /// 通过key获取hash值后对其求余获取对应分片
    fn shard(&self, key: &K) -> Arc<Mutex<LruCache<K, V>>> {
        let mut hasher = self.hasher.build_hasher();
        key.hash(&mut hasher);
        Arc::clone(&self.sharding_vec[hasher.finish() as usize % self.sharding_size()])
    }
}

impl<K: Hash + Eq + PartialEq, V> LruCache<K, V> {
    pub(crate) fn new(cap: usize) -> Result<Self> {
        if cap < 1 {
            return Err(CacheError::CacheSizeOverFlow)
        }

        Ok(Self {
            head: None,
            tail: None,
            inner: HashMap::new(),
            cap,
            marker:PhantomData,
        })
    }

    /// 移除节点
    fn detach(&mut self, mut node: NodeReadPtr<K, V>) {
        unsafe {
            match node.as_mut().prev {
                Some(mut prev) => {
                    prev.as_mut().next = node.as_ref().next;
                }
                None => {
                    self.head = node.as_ref().next;
                }
            }
            match node.as_mut().next {
                Some(mut next) => {
                    next.as_mut().prev = node.as_ref().prev;
                }
                None => {
                    self.tail = node.as_ref().prev;
                }
            }

            node.as_mut().prev = None;
            node.as_mut().next = None;
        }
    }

    /// 添加节点至头部
    fn attach(&mut self, mut node: NodeReadPtr<K, V>) {
        match self.head {
            Some(mut head) => {
                unsafe {
                    head.as_mut().prev = Some(node);
                    node.as_mut().next = Some(head);
                    node.as_mut().prev = None;
                }
                self.head = Some(node);
            }
            None => {
                unsafe {
                    node.as_mut().prev = None;
                    node.as_mut().next = None;
                }
                self.head = Some(node);
                self.tail = Some(node);
            }
        }
    }

    /// 判断并驱逐节点
    fn expulsion(&mut self) {
        if let Some(tail) = self.tail {
            if self.inner.len() >= self.cap {
                self.detach(tail);
                let _ignore = self.inner.remove(&KeyRef(tail));
            }
        }
    }

    pub(crate) fn put(&mut self, key: K, value: V) -> Option<V> {
        let node = NodeReadPtr(Box::leak(Box::new(Node::new(key, value))).into());
        let old_node = self.inner.remove(&KeyRef(node))
            .map(|node| {
                self.detach(node);
                node
            });
        self.expulsion();
        self.attach(node);
        let _ignore1 = self.inner.insert(KeyRef(node), node);
        old_node.map(|node| unsafe {
            let node: Box<Node<K, V>> = Box::from_raw(node.as_ptr());
            node.value
        })
    }

    #[allow(dead_code)]
    fn get_node(&mut self, key: &K) -> Option<NodeReadPtr<K, V>> {
        if let Some(node) = self.inner.get(key) {
            let node = *node;
            self.detach(node);
            self.attach(node);
            Some(node)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get(&mut self, key: &K) -> Option<&V> {
        if let Some(node) = self.inner.get(key) {
            let node = *node;
            self.detach(node);
            self.attach(node);
            unsafe { Some(&node.as_ref().value) }
        } else {
            None
        }
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        self.inner.remove(key)
            .map(|node| {
                self.detach(node);
                unsafe {
                    let node: Box<Node<K, V>> = Box::from_raw(node.as_ptr());
                    node.value
                }
            })
    }

    fn get_or_insert_node<F>(
        &mut self,
        key: K,
        fn_once: F
    ) -> Result<NodeReadPtr<K, V>>
        where F: FnOnce(&K) -> Result<V>
    {
        if let Some(node) = self.inner.get(&key) {
            let node = *node;
            self.detach(node);
            self.attach(node);
            Ok(node)
        } else {
            let value = fn_once(&key)?;
            let node = NodeReadPtr(Box::leak(Box::new(Node::new(key, value))).into());
            let _ignore = self.inner.remove(&KeyRef(node))
                .map(|node| {
                    self.detach(node);
                    node
                });
            self.expulsion();
            self.attach(node);
            let _ignore1 = self.inner.insert(KeyRef(node), node);
            Ok(node)
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get_or_insert<F>(
        &mut self,
        key: K,
        fn_once: F
    ) -> Result<&V>
        where F: FnOnce(&K) -> Result<V>
    {
        self.get_or_insert_node(key, fn_once)
            .map(|node| unsafe { &node.as_ref().value })
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    #[allow(dead_code)]
    pub(crate) fn iter(&self) -> LruCacheIter<K, V> {
        LruCacheIter {
            inner: self.inner.iter(),
        }
    }
}

pub(crate) struct LruCacheIter<'a, K, V> {
    inner: Iter<'a, KeyRef<K, V>, NodeReadPtr<K, V>>
}

impl<'a, K, V> Iterator for LruCacheIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
            .map(|(_, node)| unsafe { (&node.as_ref().key, &node.as_ref().value) })
    }
}

impl<K, V> Drop for LruCache<K, V> {
    #[allow(clippy::drop_copy)]
    fn drop(&mut self) {
        while let Some(node) = self.head.take(){
            unsafe {
                self.head = node.as_ref().next;
                drop(node.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;
    use std::collections::HashSet;
    use crate::kernel::utils::lru_cache::{LruCache, ShardingLruCache};

    #[test]
    fn test_lru_cache() {
        let mut lru = LruCache::new(3).unwrap();
        assert!(lru.is_empty());
        assert_eq!(lru.put(1, 10), None);
        assert_eq!(lru.put(2, 20), None);
        assert_eq!(lru.put(3, 30), None);
        assert_eq!(lru.get(&1), Some(&10));
        assert_eq!(lru.put(2, 200), Some(20));
        assert_eq!(lru.put(4, 40), None);
        assert_eq!(lru.get(&2), Some(&200));
        assert_eq!(lru.get(&3), None);

        assert_eq!(
            lru.get_or_insert(
                9,
                |_| Ok(9)
            ).unwrap(),
            &9
        );

        assert_eq!(lru.len(), 3);
        assert!(!lru.is_empty());

        let mut set = HashSet::from([(&9, &9), (&2, &200), (&4, &40)]);

        for item in lru.iter() {
            assert!(set.remove(&item))
        }
    }

    #[test]
    fn test_sharding_cache() {
        let lru = ShardingLruCache::new(4, 2, RandomState::default()).unwrap();
        assert!(lru.is_empty());
        assert_eq!(lru.put(1, 10), None);
        assert_eq!(lru.get(&1), Some(&10));
        assert!(!lru.is_empty());
        assert_eq!(
            lru.get_or_insert(
                9,
                |_| Ok(9)
            ).unwrap(),
            &9
        );
    }
}